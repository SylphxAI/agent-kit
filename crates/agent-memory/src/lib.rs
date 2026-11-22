//! Pure-Rust Raw-RAG memory engine over immutable raw conversation and event history.
//!
//! Raw history remains the sole source of truth: retrieval combines lexical BM25
//! search with dense embeddings via reciprocal rank fusion ([`fusion::reciprocal_rank`])
//! and optional cross-encoder reranking, returning verified exact raw-byte citations
//! authenticated by SHA-256 digests.
//!
//! Memory is event history and learned notes, not a curated Knowledge catalog.
//! Keep recalled material separate from the Instructions governing an agent.
//!
//! ## Backends and features
//!
//! - `sqlite` (default): local `SqliteStore` with FTS5 BM25 and int8-quantized dense search.
//! - `postgres`: `PostgresPgvectorStore` with PostgreSQL `tsvector`, `pgvector` cosine
//!   similarity, SQL reciprocal rank fusion, and exact byte-slice citation readback.
//! - `service`: local ONNX Runtime inference, remote embedding and reranking
//!   providers, background dense indexing, and offline retrieval benchmarking.
//!
//! ## Append and read exact bytes without a database
//!
//! ```
//! use agent_memory::{RawSegment, read_raw};
//!
//! # fn main() -> std::io::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let segment = RawSegment::open(&dir.path().join("events.raw"))?;
//! let event = br#"{ "text": "garden archive" }"#;
//! let citation = segment.append(event)?;
//! assert_eq!(citation.length, event.len() as u64);
//! // Reads the exact bytes at the coordinate and verifies its SHA-256 digest.
//! assert_eq!(read_raw(&citation)?, event);
//! # Ok(())
//! # }
//! ```

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
