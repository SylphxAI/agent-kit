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

/// Document-side tokenizer: emits lowercased alphanumeric/identifier tokens
/// (`len >= 2`, plus sub-tokens split on `_` and `-`), and both CJK unigrams and
/// overlapping bigrams. This supports single-character recall and compound phrases.
pub fn tokenize_hybrid_search_tokens(text: &str) -> Vec<String> {
    scan_tokens(
        text,
        is_cjk,
        |token| token,
        |run, out| {
            out.extend(run.iter().map(|ch| ch.to_string()));
            out.extend(cjk_bigrams(run));
        },
    )
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SearchTerm {
    Token(String),
    CjkPhrase(Vec<String>),
}

/// A multi-character CJK run matches only its ordered, adjacent bigrams. A
/// phrase of unigrams could match separately delimited characters in the
/// existing index; OR-ing bigrams could match only part of a longer compound.
pub(crate) fn tokenize_search_terms(text: &str) -> Vec<SearchTerm> {
    scan_tokens(
        text,
        // The indexed script ranges also contain punctuation such as ？ and ・.
        // Treat it as a query boundary without changing existing index tokens.
        |ch| is_cjk(ch) && ch.is_alphanumeric(),
        SearchTerm::Token,
        |run, out| {
            out.push(match run {
                [ch] => SearchTerm::Token(ch.to_string()),
                run => SearchTerm::CjkPhrase(cjk_bigrams(run).collect()),
            });
        },
    )
}

fn scan_tokens<T>(
    text: &str,
    cjk_char: impl Fn(char) -> bool,
    ascii_term: impl Fn(String) -> T,
    cjk_terms: impl Fn(&[char], &mut Vec<T>),
) -> Vec<T> {
    let mut tokens = Vec::new();
    let mut ascii_buf = String::new();
    let mut cjk_run = Vec::new();
    let flush_cjk = |run: &mut Vec<char>, out: &mut Vec<T>| {
        if !run.is_empty() {
            cjk_terms(run, out);
            run.clear();
        }
    };
    for ch in text.chars() {
        if cjk_char(ch) {
            flush_ascii_tokens(&mut ascii_buf, &mut tokens, &ascii_term);
            cjk_run.push(ch);
        } else if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            flush_cjk(&mut cjk_run, &mut tokens);
            ascii_buf.push(ch);
        } else {
            flush_ascii_tokens(&mut ascii_buf, &mut tokens, &ascii_term);
            flush_cjk(&mut cjk_run, &mut tokens);
        }
    }
    flush_ascii_tokens(&mut ascii_buf, &mut tokens, &ascii_term);
    flush_cjk(&mut cjk_run, &mut tokens);
    tokens
}

fn flush_ascii_tokens<T>(buf: &mut String, out: &mut Vec<T>, term: &impl Fn(String) -> T) {
    let trimmed = buf.trim_matches(['-', '_']);
    if trimmed.len() >= 2 {
        let lower = trimmed.to_ascii_lowercase();
        out.push(term(lower.clone()));
        if lower.contains('_') || lower.contains('-') {
            for part in lower.split(['_', '-']) {
                if part.len() >= 2 {
                    out.push(term(part.to_string()));
                }
            }
        }
    }
    buf.clear();
}

fn cjk_bigrams(run: &[char]) -> impl Iterator<Item = String> + '_ {
    run.windows(2).map(|pair| {
        let mut s = String::with_capacity(6);
        s.push(pair[0]);
        s.push(pair[1]);
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_tokens_keep_existing_index_layout_and_boundaries() {
        assert_eq!(
            tokenize_hybrid_search_tokens("__Deploy_Staging-v2-部署失敗、頭"),
            [
                "deploy_staging-v2",
                "deploy",
                "staging",
                "v2",
                "部",
                "署",
                "失",
                "敗",
                "部署",
                "署失",
                "失敗",
                "頭",
            ]
        );
    }

    #[test]
    fn multi_character_query_runs_are_ordered_bigram_phrases() {
        use SearchTerm::CjkPhrase;
        assert_eq!(
            tokenize_search_terms("頭像"),
            [CjkPhrase(vec!["頭像".into()])]
        );
        assert_eq!(
            tokenize_search_terms("由頭跟到尾"),
            [CjkPhrase(
                ["由頭", "頭跟", "跟到", "到尾"].map(String::from).to_vec()
            )]
        );
    }

    #[test]
    fn query_keeps_ascii_terms_and_standalone_cjk_at_run_boundaries() {
        use SearchTerm::{CjkPhrase, Token};
        assert_eq!(
            tokenize_search_terms("頭像圖Deploy_Staging-v2、頭 help！"),
            [
                CjkPhrase(vec!["頭像".into(), "像圖".into()]),
                Token("deploy_staging-v2".into()),
                Token("deploy".into()),
                Token("staging".into()),
                Token("v2".into()),
                Token("頭".into()),
                Token("help".into()),
            ]
        );
        assert!(tokenize_search_terms("a - !").is_empty());
    }

    #[test]
    fn history_sort_parses_aliases() {
        assert_eq!(HistorySort::parse(Some("newest")), HistorySort::Newest);
        assert_eq!(HistorySort::parse(Some("asc")), HistorySort::Oldest);
        assert_eq!(HistorySort::parse(None), HistorySort::Relevance);
    }
}
