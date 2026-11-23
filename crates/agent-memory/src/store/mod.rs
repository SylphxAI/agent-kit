//! Pluggable storage backend trait ([`MemoryStore`]) for Raw-RAG memory engines.
//!
//! Implementations store immutable raw bytes as the sole source of truth,
//! index lexical and dense projections derived from those bytes, fuse lexical
//! and dense candidates via reciprocal rank fusion, and enforce owner ACLs plus
//! SHA-256 citation integrity on every readback.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::{
    fusion::{self, K as RRF_K},
    tokenize::{HistorySearchQuery, IndexMetadata, MemoryScope},
};

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;

#[cfg(feature = "postgres")]
pub use postgres::PostgresPgvectorStore;

/// Errors returned by [`MemoryStore`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("integrity mismatch: {0}")]
    IntegrityMismatch(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[cfg(feature = "sqlite")]
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[cfg(feature = "sqlite")]
    #[error("index error: {0}")]
    Index(#[from] crate::host_index::IndexError),
    #[cfg(feature = "postgres")]
    #[error("postgres error: {0}")]
    Postgres(#[from] sqlx::Error),
    #[error("backend error: {0}")]
    Backend(String),
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
fn immutable_record_conflict(id: &str) -> StoreError {
    StoreError::IntegrityMismatch(format!(
        "immutable record {id} already exists with a different owner or payload"
    ))
}

/// An immutable raw event or document appended to a [`MemoryStore`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawRecordInput {
    pub id: String,
    pub agent_id: String,
    /// Must be a concrete source ([`MemoryScope::Memory`] or [`MemoryScope::Transcripts`]).
    pub source: MemoryScope,
    pub timestamp_ms: Option<i64>,
    pub direction: String,
    pub sender: String,
    pub destination: String,
    pub role: Option<String>,
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_names: Vec<String>,
    /// Exact immutable raw bytes (JSON event, Markdown topic, or arbitrary UTF-8/binary payload).
    pub raw_bytes: Vec<u8>,
}

/// Verified exact-byte citation referencing a slice inside an immutable record.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawCitation {
    pub document_id: String,
    pub agent_id: String,
    /// Byte offset relative to the start of the record's raw bytes.
    pub offset: u64,
    /// Exact byte length of the cited slice.
    pub length: u64,
    /// SHA-256 of the cited slice (`raw_bytes[offset..offset+length]`).
    pub sha256: String,
    /// SHA-256 of the complete immutable raw record.
    pub raw_sha256: String,
    /// Monotonic sequence number within the backing store/segment.
    pub seq: u64,
}

/// A dense embedding chunk anchored to an exact raw byte slice inside an immutable record.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EmbeddingChunk {
    pub piece_index: u32,
    /// Byte offset relative to the start of the record's raw bytes.
    pub offset: u64,
    /// Byte length of the cited slice.
    pub length: u64,
    /// SHA-256 of `raw_bytes[offset..offset+length]`.
    pub sha256: String,
    /// SHA-256 of the projected text embedded to produce `vector`.
    pub text_sha256: String,
    /// L2-normalized embedding vector.
    pub vector: Vec<f32>,
}

/// One retrieval hit returned by lexical, dense, or hybrid Raw-RAG search.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub document_id: String,
    pub metadata: IndexMetadata,
    pub citation: RawCitation,
    /// Authenticated exact raw bytes of the cited slice.
    pub raw_slice: Vec<u8>,
    /// Lexical BM25 / full-text score when lexical retrieval contributed.
    pub lexical_score: Option<f64>,
    /// Dense cosine similarity score when dense retrieval contributed.
    pub dense_score: Option<f32>,
    /// Final ranking score (higher is more relevant).
    pub score: f64,
}

/// Unified search request passed to [`MemoryStore`].
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MemorySearchRequest {
    /// Authorized owner agent IDs. An empty slice ALWAYS fails closed with zero hits.
    pub owners: Vec<String>,
    pub scope: MemoryScope,
    pub query: HistorySearchQuery,
    /// Optional L2-normalized query vector for dense or hybrid RRF search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_vector: Option<Vec<f32>>,
    /// Vector space identity (model + dimensions + preprocessing). Defaults to `"default"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_space: Option<String>,
}

impl MemorySearchRequest {
    /// Effective vector space key (`"default"` when unspecified).
    pub fn effective_vector_space(&self) -> &str {
        self.vector_space
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("default")
    }

    /// Effective clamped hit limit (`1..=200`).
    pub fn effective_limit(&self) -> usize {
        self.query.limit.clamp(1, 200)
    }
}

/// Basic record and vector counts reported by a [`MemoryStore`].
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoreStats {
    pub documents: u64,
    pub vectors: u64,
}

/// Pluggable storage backend for Raw-RAG memory engines.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Canonical backend name (e.g., `"sqlite"`, `"postgres-pgvector"`).
    fn backend_name(&self) -> &'static str;

    /// Ensure schema and indexes exist (idempotent).
    async fn migrate(&self) -> Result<(), StoreError>;

    /// Append an immutable raw record and index it for lexical search.
    /// An existing ID can only be retried by the same owner with identical raw
    /// bytes; this returns the original citation without updating metadata.
    /// A different owner or payload fails with [`StoreError::IntegrityMismatch`].
    async fn append_record(&self, record: &RawRecordInput) -> Result<RawCitation, StoreError>;

    /// Store or replace dense embedding chunks for an existing record within `vector_space`.
    async fn upsert_embeddings(
        &self,
        document_id: &str,
        vector_space: &str,
        chunks: &[EmbeddingChunk],
    ) -> Result<(), StoreError>;

    /// Lexical BM25 / full-text search within `request.owners`.
    async fn search_lexical(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<SearchHit>, StoreError>;

    /// Dense cosine similarity search within `request.owners`.
    async fn search_dense(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<SearchHit>, StoreError>;

    /// Hybrid Raw-RAG retrieval: BM25 + dense vector search fused with Reciprocal Rank Fusion.
    /// Falls back cleanly to lexical-only retrieval if `request.query_vector` is `None`.
    async fn search_hybrid(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<SearchHit>, StoreError> {
        if request.owners.is_empty() {
            return Ok(Vec::new());
        }
        let limit = request.effective_limit();
        let candidate_limit = (limit.saturating_mul(3)).clamp(limit, 200);
        let mut candidate_req = request.clone();
        candidate_req.query.limit = candidate_limit;

        let lexical_hits = self.search_lexical(&candidate_req).await?;
        let Some(_) = request.query_vector.as_ref().filter(|v| !v.is_empty()) else {
            return Ok(lexical_hits.into_iter().take(limit).collect());
        };
        let dense_hits = self.search_dense(&candidate_req).await?;
        if dense_hits.is_empty() {
            return Ok(lexical_hits.into_iter().take(limit).collect());
        }
        if lexical_hits.is_empty() {
            return Ok(dense_hits.into_iter().take(limit).collect());
        }
        Ok(fuse_search_hits(lexical_hits, dense_hits, limit))
    }

    /// Exact raw-byte citation readback authenticated by owner ACL and SHA-256.
    /// Returns `Ok(None)` if the record does not exist or is outside `owners`.
    async fn read_citation(
        &self,
        owners: &[String],
        document_id: &str,
        offset: u64,
        length: u64,
        expected_sha256: Option<&str>,
    ) -> Result<Option<(RawCitation, Vec<u8>)>, StoreError>;

    /// Delete one record and its derived lexical/dense rows within `owners`.
    async fn delete_record(&self, owners: &[String], document_id: &str)
    -> Result<bool, StoreError>;

    /// Return aggregate document and vector counts.
    async fn stats(&self) -> Result<StoreStats, StoreError>;
}

/// Deterministically fuse lexical and dense [`SearchHit`] lists using Reciprocal Rank Fusion.
pub fn fuse_search_hits(
    lexical_hits: Vec<SearchHit>,
    dense_hits: Vec<SearchHit>,
    limit: usize,
) -> Vec<SearchHit> {
    let lex_ids: Vec<String> = lexical_hits.iter().map(|h| h.document_id.clone()).collect();
    let dense_ids: Vec<String> = dense_hits.iter().map(|h| h.document_id.clone()).collect();

    let mut by_id: HashMap<String, SearchHit> = HashMap::new();
    for hit in lexical_hits {
        by_id.insert(hit.document_id.clone(), hit);
    }
    for hit in dense_hits {
        by_id
            .entry(hit.document_id.clone())
            .and_modify(|existing| {
                existing.dense_score = hit.dense_score;
            })
            .or_insert(hit);
    }

    let fused = fusion::reciprocal_rank(&[lex_ids, dense_ids], RRF_K);
    fused
        .into_iter()
        .take(limit)
        .filter_map(|(id, rrf_score)| {
            let mut hit = by_id.remove(&id)?;
            hit.score = rrf_score;
            Some(hit)
        })
        .collect()
}
