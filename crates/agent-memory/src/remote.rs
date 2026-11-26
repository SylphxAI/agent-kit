//! JSON wire codecs for remote Raw-RAG embeddings and reranking.
//!
//! Available without backend or service features. Callers own transport, credentials,
//! request/response size limits, vector-space selection, and usage accounting.

use serde_json::{Map, Value, json};

pub use crate::Stamp;

/// A malformed or incomplete provider response. Diagnostics never include provider text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WireError {
    #[error("remote response has no result array")]
    MissingResults,
    #[error("remote response has {actual} results for {expected} inputs")]
    ResultCount { expected: usize, actual: usize },
    #[error("remote response row {row} has an invalid or out-of-range index")]
    InvalidIndex { row: usize },
    #[error("remote response row {row} repeats a result index")]
    DuplicateIndex { row: usize },
    #[error("remote embedding row {row} has a missing, empty, or invalid vector")]
    InvalidEmbedding { row: usize },
    #[error("remote embedding row {row} has a different vector length")]
    InconsistentDimensions { row: usize },
    #[error("remote rerank row {row} has a missing or invalid score")]
    InvalidScore { row: usize },
}

/// Build an OpenAI-compatible embedding request with float encoding.
/// Set `openrouter` to disallow provider data collection on OpenRouter.
pub fn embedding_request(
    model: &str,
    input: &[String],
    dimensions: Option<usize>,
    openrouter: bool,
) -> Value {
    let mut body = json!({"model": model, "input": input, "encoding_format": "float"});
    if let Some(dimensions) = dimensions {
        body["dimensions"] = json!(dimensions);
    }
    if openrouter {
        deny_data_collection(&mut body);
    }
    body
}

/// Build a reranking request for all passages, without modifying the query or passages.
/// Set `openrouter` to disallow provider data collection on OpenRouter.
pub fn rerank_request(model: &str, query: &str, passages: &[String], openrouter: bool) -> Value {
    let mut body = json!({
        "model": model,
        "query": query,
        "documents": passages,
        "top_n": passages.len(),
    });
    if openrouter {
        deny_data_collection(&mut body);
    }
    body
}

pub(crate) fn deny_data_collection(body: &mut Value) {
    let Value::Object(object) = body else {
        return;
    };
    let provider = object
        .entry("provider")
        .or_insert_with(|| Value::Object(Map::new()));
    if !provider.is_object() {
        *provider = Value::Object(Map::new());
    }
    provider["data_collection"] = json!("deny");
}

/// Decode `data` into one nonempty float vector per input, in input order.
/// Indices must be a permutation of `0..count` and vector lengths must be uniform.
/// The caller must check the expected dimension for its vector space.
pub fn embedding_response(response: &Value, count: usize) -> Result<Vec<Vec<f32>>, WireError> {
    let mut dimensions = None;
    decode_indexed(response.get("data"), count, |row, value| {
        let embedding = value
            .get("embedding")
            .and_then(Value::as_array)
            .filter(|vector| !vector.is_empty())
            .ok_or(WireError::InvalidEmbedding { row })?;
        if *dimensions.get_or_insert(embedding.len()) != embedding.len() {
            return Err(WireError::InconsistentDimensions { row });
        }
        embedding
            .iter()
            .map(|component| finite_f32(component).ok_or(WireError::InvalidEmbedding { row }))
            .collect()
    })
}

/// Decode `results` or `data` into one score per passage, in input order.
/// Indices must be a permutation of `0..count`; `relevance_score` and `score` are supported.
pub fn rerank_response(response: &Value, count: usize) -> Result<Vec<f32>, WireError> {
    decode_indexed(
        response.get("results").or_else(|| response.get("data")),
        count,
        |row, value| {
            value
                .get("relevance_score")
                .or_else(|| value.get("score"))
                .and_then(finite_f32)
                .ok_or(WireError::InvalidScore { row })
        },
    )
}

fn decode_indexed<T>(
    rows: Option<&Value>,
    count: usize,
    mut decode: impl FnMut(usize, &Value) -> Result<T, WireError>,
) -> Result<Vec<T>, WireError> {
    let rows = rows
        .and_then(Value::as_array)
        .ok_or(WireError::MissingResults)?;
    if rows.len() != count {
        return Err(WireError::ResultCount {
            expected: count,
            actual: rows.len(),
        });
    }
    let mut ordered = Vec::with_capacity(count);
    ordered.resize_with(count, || None);
    for (row, value) in rows.iter().enumerate() {
        let slot = value
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| ordered.get_mut(index))
            .ok_or(WireError::InvalidIndex { row })?;
        if slot.is_some() {
            return Err(WireError::DuplicateIndex { row });
        }
        *slot = Some(decode(row, value)?);
    }
    // Exact cardinality and unique in-range indices guarantee every slot is filled.
    Ok(ordered.into_iter().flatten().collect())
}

fn finite_f32(value: &Value) -> Option<f32> {
    let value = value.as_f64()?;
    (f64::from(f32::MIN)..=f64::from(f32::MAX))
        .contains(&value)
        .then_some(value as f32)
}

/// Return reported input usage only; absent or invalid fields are not estimated.
/// Prefer explicit prompt/input tokens or Cohere billed input units, then reported
/// total tokens for embedding and reranking APIs that expose only a total.
pub fn reported_input_tokens(response: &Value) -> Option<u64> {
    [
        "/usage/prompt_tokens",
        "/usage/input_tokens",
        "/meta/billed_units/input_tokens",
        "/usage/total_tokens",
    ]
    .into_iter()
    .find_map(|path| response.pointer(path).and_then(Value::as_u64))
}

/// Return the reported nonnegative, finite `usage.cost`, if available.
pub fn reported_cost(response: &Value) -> Option<f64> {
    response
        .pointer("/usage/cost")
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}
