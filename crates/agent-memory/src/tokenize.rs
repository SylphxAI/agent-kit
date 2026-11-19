//! Hybrid ASCII + CJK tokenization and structured Raw-RAG query/metadata types.

use serde::{Deserialize, Serialize};

use crate::raw::RawCoordinate;
use crate::text::is_cjk;

/// Retrieval source scope over indexed raw history and memory notes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    #[default]
    All,
    Memory,
    Transcripts,
}

impl MemoryScope {
    /// Canonical storage label for a single source (`None` for [`Self::All`]).
    pub fn as_source(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Memory => Some("memory"),
            Self::Transcripts => Some("transcripts"),
        }
    }

    /// Parse a stored source label back into a [`MemoryScope`].
    pub fn from_source(raw: &str) -> Self {
        match raw {
            "memory" => Self::Memory,
            _ => Self::Transcripts,
        }
    }
}

/// Ordering for history search results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistorySort {
    #[default]
    Relevance,
    Newest,
    Oldest,
}

impl HistorySort {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("newest" | "desc" | "time_desc" | "recent" | "latest") => Self::Newest,
            Some("oldest" | "asc" | "time_asc" | "chronological") => Self::Oldest,
            _ => Self::Relevance,
        }
    }
}

/// Structured + lexical search request over immutable raw conversation and tool history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySearchQuery {
    /// Primary search query string (optional when filtering by time, sequence, peer, or tool).
    pub query: Option<String>,
    /// Additional search queries (OR-combined with `query` for multi-keyword / synonym recall).
    #[serde(default)]
    pub queries: Vec<String>,
    /// Negative terms — any record containing any of these substrings (case-insensitive) is excluded.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Optional agent selector narrowing inside the authorized owner set.
    #[serde(default)]
    pub agent: Option<String>,
    /// Optional peer filter (sender or destination).
    #[serde(default)]
    pub with_peer: Option<String>,
    /// Direction filter: `"in"`, `"out"`, or `None` / `"all"`.
    #[serde(default)]
    pub direction: Option<String>,
    /// Optional sender substring filter.
    #[serde(default)]
    pub sender: Option<String>,
    /// Whether to include tool/system history (`true` by default).
    #[serde(default = "default_true")]
    pub include_tools: bool,
    /// When `true`, searches ONLY tool/system history.
    #[serde(default)]
    pub only_tools: bool,
    /// Optional tool name filter (e.g. `"workspace"`, `"web"`, `"memory"`).
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Lower timestamp bound (`timestamp_ms > since_ms`).
    #[serde(default)]
    pub since_ms: Option<i64>,
    /// Upper timestamp bound (`timestamp_ms <= until_ms`).
    #[serde(default)]
    pub until_ms: Option<i64>,
    /// Optional sequence lower bound (`seq >= from_seq`).
    #[serde(default)]
    pub from_seq: Option<i64>,
    /// Optional sequence upper bound (`seq <= to_seq`).
    #[serde(default)]
    pub to_seq: Option<i64>,
    /// Result ordering (`Relevance`, `Newest`, `Oldest`).
    #[serde(default)]
    pub sort: HistorySort,
    /// Contiguous neighbor turns (`0..=10`) to include around each matched hit.
    #[serde(default)]
    pub radius: usize,
    /// Maximum hits to return (`1..=200`).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Caller-authorized immutable owner IDs. `None` lets the store method's explicit
    /// owner argument control visibility.
    #[serde(default)]
    pub visible_agent_ids: Option<Vec<String>>,
}

fn default_true() -> bool {
    true
}

fn default_limit() -> usize {
    20
}

impl Default for HistorySearchQuery {
    fn default() -> Self {
        Self {
            query: None,
            queries: Vec::new(),
            exclude: Vec::new(),
            agent: None,
            with_peer: None,
            direction: None,
            sender: None,
            include_tools: true,
            only_tools: false,
            tool_name: None,
            since_ms: None,
            until_ms: None,
            from_seq: None,
            to_seq: None,
            sort: HistorySort::Relevance,
            radius: 0,
            limit: default_limit(),
            visible_agent_ids: None,
        }
    }
}

/// Metadata indexed alongside an immutable raw document.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexMetadata {
    /// Stable owner/agent ID, never a mutable display name.
    pub agent_id: String,
    pub source: MemoryScope,
    /// Source event timestamp in milliseconds since Unix epoch, if known.
    pub timestamp_ms: Option<i64>,
    pub direction: String,
    pub sender: String,
    pub destination: String,
    pub tool_name: Option<String>,
    /// All tool names present in a multi-tool event.
    #[serde(default)]
    pub tool_names: Vec<String>,
    pub role: Option<String>,
}

/// One lexical search hit with verified raw-byte coordinates.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IndexHit {
    pub id: String,
    pub metadata: IndexMetadata,
    /// The whole lossless event/document coordinate.
    pub raw: RawCoordinate,
    /// Exact bounded matching window coordinate with its own SHA-256.
    pub window: RawCoordinate,
    pub lexical_score: f64,
}

/// A raw byte range another retriever cites inside an indexed document:
/// absolute offsets in the document's segment and the range's SHA-256.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitedRange {
    pub document_id: String,
    pub offset: u64,
    pub length: u64,
    pub sha256: String,
}

/// Hybrid ASCII + CJK tokenizer: emits lowercased alphanumeric/identifier tokens
/// (`len >= 2`, plus sub-tokens split on `_` and `-`) together with individual CJK
/// characters and CJK bigrams so English, code identifiers, and CJK queries
/// achieve high BM25 recall without external stemmers.
pub fn tokenize_hybrid_search_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii_buf = String::new();
    let mut cjk_run: Vec<char> = Vec::new();

    let flush_ascii = |buf: &mut String, out: &mut Vec<String>| {
        let trimmed = buf.trim_matches(['-', '_']);
        if trimmed.len() >= 2 {
            let lower = trimmed.to_ascii_lowercase();
            out.push(lower.clone());
            if lower.contains('_') || lower.contains('-') {
                for part in lower.split(['_', '-']) {
                    if part.len() >= 2 {
                        out.push(part.to_string());
                    }
                }
            }
        }
        buf.clear();
    };
    let flush_cjk = |run: &mut Vec<char>, out: &mut Vec<String>| {
        for &ch in run.iter() {
            out.push(ch.to_string());
        }
        for pair in run.windows(2) {
            let mut s = String::with_capacity(6);
            s.push(pair[0]);
            s.push(pair[1]);
            out.push(s);
        }
        run.clear();
    };

    for ch in text.chars() {
        if is_cjk(ch) {
            flush_ascii(&mut ascii_buf, &mut tokens);
            cjk_run.push(ch);
        } else if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            flush_cjk(&mut cjk_run, &mut tokens);
            ascii_buf.push(ch);
        } else {
            flush_ascii(&mut ascii_buf, &mut tokens);
            flush_cjk(&mut cjk_run, &mut tokens);
        }
    }
    flush_ascii(&mut ascii_buf, &mut tokens);
    flush_cjk(&mut cjk_run, &mut tokens);
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_tokenizer_splits_identifiers_and_emits_cjk_bigrams() {
        let tokens = tokenize_hybrid_search_tokens("deploy_staging-v2 部署失敗!");
        assert!(tokens.contains(&"deploy_staging-v2".to_string()));
        assert!(tokens.contains(&"deploy".to_string()));
        assert!(tokens.contains(&"staging".to_string()));
        assert!(tokens.contains(&"v2".to_string()));
        assert!(tokens.contains(&"部".to_string()));
        assert!(tokens.contains(&"部署".to_string()));
        assert!(tokens.contains(&"署失".to_string()));
        assert!(tokens.contains(&"失敗".to_string()));
    }

    #[test]
    fn history_sort_parses_aliases() {
        assert_eq!(HistorySort::parse(Some("newest")), HistorySort::Newest);
        assert_eq!(HistorySort::parse(Some("asc")), HistorySort::Oldest);
        assert_eq!(HistorySort::parse(None), HistorySort::Relevance);
    }
}
