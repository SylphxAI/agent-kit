//! Pure-Rust Raw-RAG memory engine over immutable raw conversation and event history.
//!
//! Raw history remains the sole source of truth: retrieval combines lexical BM25
//! search with dense embeddings via reciprocal rank fusion ([`fusion::reciprocal_rank`])
//! and optional cross-encoder reranking, returning verified exact raw-byte citations
//! authenticated by SHA-256 digests.
//!
//! ## Backends & Feature Flags
//! - `sqlite` (default): local [`store::SqliteStore`], [`host_index::HostIndex`] (FTS5 BM25),
//!   and [`dense::DenseIndex`] (per-vector int8 quantized cosine search).
//! - `postgres`: production [`store::PostgresPgvectorStore`] using PostgreSQL `tsvector` +
//!   `pgvector` cosine similarity + SQL CTE reciprocal rank fusion and exact byte-slice
//!   citation readback.
//! - `service`: local ONNX Runtime inference (`runtime`, `model`, `pins`), remote OpenAI /
//!   Cohere / Voyage / Jina providers (`providers`), striped background dense indexing
//!   (`embed`, `service`), and offline retrieval benchmarking (`eval`).

pub mod fusion;
pub mod notes;
pub mod protocol;
pub mod raw;
pub mod store;
pub mod text;
pub mod tokenize;

#[cfg(feature = "sqlite")]
pub mod dense;
#[cfg(feature = "sqlite")]
pub mod host_index;

#[cfg(feature = "service")]
pub mod embed;
#[cfg(feature = "service")]
pub mod eval;
#[cfg(feature = "service")]
pub mod model;
#[cfg(feature = "service")]
pub mod pins;
#[cfg(feature = "service")]
pub mod providers;
#[cfg(feature = "service")]
pub mod runtime;
#[cfg(feature = "service")]
pub mod service;

pub use fusion::{K as RRF_K, reciprocal_rank};
pub use raw::{MAX_RAW_RANGE_BYTES, RawCoordinate, RawSegment, read_raw, read_raw_range};
pub use store::{
    EmbeddingChunk, MemorySearchRequest, MemoryStore, RawCitation, RawRecordInput, SearchHit,
    StoreError, StoreStats,
};
pub use tokenize::{
    CitedRange, HistorySearchQuery, HistorySort, IndexHit, IndexMetadata, MemoryScope,
    tokenize_hybrid_search_tokens,
};

#[cfg(feature = "sqlite")]
pub use store::SqliteStore;

#[cfg(feature = "postgres")]
pub use store::PostgresPgvectorStore;

/// A temporary directory under the physical temporary root. Raw reads refuse
/// a path through a symbolic link, and macOS keeps its temporary directory
/// behind one (`/var` is `/private/var`).
#[cfg(test)]
pub(crate) fn test_tempdir() -> tempfile::TempDir {
    let base = std::env::temp_dir()
        .canonicalize()
        .expect("physical temporary root");
    tempfile::tempdir_in(base).expect("owned temporary fixture")
}
