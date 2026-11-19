//! SQLite + FTS5 + int8 quantized dense implementation of [`MemoryStore`].

use async_trait::async_trait;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::{
    dense::{DenseIndex, Document, Piece, quantize},
    host_index::{HostIndex, INDEX_FILE},
    raw::{MAX_RAW_RANGE_BYTES, RawCoordinate, RawSegment, read_raw, read_raw_range, sha256_hex},
    store::{
        EmbeddingChunk, MemorySearchRequest, MemoryStore, RawCitation, RawRecordInput, SearchHit,
        StoreError, StoreStats,
    },
    tokenize::{IndexMetadata, MemoryScope},
};

/// Production local [`MemoryStore`] backed by append-only [`RawSegment`] files,
/// SQLite FTS5 BM25 ([`HostIndex`]), and per-space int8 quantized [`DenseIndex`] files.
pub struct SqliteStore {
    root: PathBuf,
    state: Mutex<SqliteState>,
}

struct SqliteState {
    host: HostIndex,
    segments: HashMap<String, RawSegment>,
    dense_spaces: HashMap<(String, usize), DenseIndex>,
}

impl SqliteStore {
    /// Open or create a [`SqliteStore`] rooted at `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("segments"))?;
        std::fs::create_dir_all(root.join("dense"))?;
        let host = HostIndex::open(&root.join(INDEX_FILE))?;
        Ok(Self {
            root,
            state: Mutex::new(SqliteState {
                host,
                segments: HashMap::new(),
                dense_spaces: HashMap::new(),
            }),
        })
    }

    /// Root directory holding segments, FTS5 host index, and dense indexes.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, SqliteState>, StoreError> {
        self.state
            .lock()
            .map_err(|_| StoreError::Backend("sqlite store lock poisoned".into()))
    }

    fn dense_file_path(&self, space: &str, dims: usize) -> PathBuf {
        let digest = sha256_hex(format!("{space}:{dims}").as_bytes());
        self.root
            .join("dense")
            .join(format!("dense-{}.db", &digest[..16]))
    }

    fn segment_path_for_agent(&self, agent_id: &str) -> PathBuf {
        let safe = sha256_hex(agent_id.as_bytes());
        self.root
            .join("segments")
            .join(format!("agent-{}.raw", &safe[..16]))
    }

    fn open_or_get_dense<'a>(
        root_dense_path: PathBuf,
        dense_spaces: &'a mut HashMap<(String, usize), DenseIndex>,
        space: &str,
        dims: usize,
    ) -> Result<&'a mut DenseIndex, StoreError> {
        let key = (space.to_owned(), dims);
        if !dense_spaces.contains_key(&key) {
            let idx = DenseIndex::open(&root_dense_path, space, dims)?;
            dense_spaces.insert(key.clone(), idx);
        }
        dense_spaces
            .get_mut(&key)
            .ok_or_else(|| StoreError::Backend("dense index disappeared".into()))
    }

    /// Synchronous convenience wrapper for [`MemoryStore::append_record`].
    pub fn append_record_sync(&self, record: &RawRecordInput) -> Result<RawCitation, StoreError> {
        if record.id.trim().is_empty() {
            return Err(StoreError::InvalidInput(
                "record id must not be empty".into(),
            ));
        }
        if record.agent_id.trim().is_empty() {
            return Err(StoreError::InvalidInput(
                "record agent_id must not be empty".into(),
            ));
        }
        if record.source == MemoryScope::All {
            return Err(StoreError::InvalidInput(
                "record source must be Memory or Transcripts, not All".into(),
            ));
        }
        let seg_path = self.segment_path_for_agent(&record.agent_id);
        let mut state = self.lock_state()?;
        if !state.segments.contains_key(&record.agent_id) {
            let segment = RawSegment::open(&seg_path)?;
            state.segments.insert(record.agent_id.clone(), segment);
        }
        let coord = state
            .segments
            .get(&record.agent_id)
            .ok_or_else(|| StoreError::Backend("segment missing".into()))?
            .append(&record.raw_bytes)?;
        let metadata = IndexMetadata {
            agent_id: record.agent_id.clone(),
            source: record.source,
            timestamp_ms: record.timestamp_ms,
            direction: record.direction.clone(),
            sender: record.sender.clone(),
            destination: record.destination.clone(),
            tool_name: record.tool_name.clone(),
            tool_names: record.tool_names.clone(),
            role: record.role.clone(),
        };
        state.host.index(&record.id, &metadata, &coord)?;
        Ok(RawCitation {
            document_id: record.id.clone(),
            agent_id: record.agent_id.clone(),
            offset: 0,
            length: coord.length,
            sha256: coord.sha256.clone(),
            raw_sha256: coord.sha256,
            seq: coord.seq,
        })
    }

    /// Synchronous convenience wrapper for [`MemoryStore::upsert_embeddings`].
    pub fn upsert_embeddings_sync(
        &self,
        document_id: &str,
        vector_space: &str,
        chunks: &[EmbeddingChunk],
    ) -> Result<(), StoreError> {
        if chunks.is_empty() {
            return Ok(());
        }
        let dims = chunks[0].vector.len();
        if dims == 0 || chunks.iter().any(|c| c.vector.len() != dims) {
            return Err(StoreError::InvalidInput(
                "embedding chunks must share a non-zero vector dimension".into(),
            ));
        }
        let dense_path = self.dense_file_path(vector_space, dims);
        let mut state = self.lock_state()?;
        // Resolve document metadata from host index via an internal unfiltered lookup.
        let doc = lookup_document_unfiltered(&state.host, document_id)?
            .ok_or_else(|| StoreError::NotFound(format!("document {document_id} not indexed")))?;
        let raw_bytes = read_raw(&doc.raw)?;
        let mut pieces = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let start = usize::try_from(chunk.offset)
                .map_err(|_| StoreError::InvalidInput("chunk offset overflow".into()))?;
            let end = start
                .checked_add(
                    usize::try_from(chunk.length)
                        .map_err(|_| StoreError::InvalidInput("chunk length overflow".into()))?,
                )
                .ok_or_else(|| StoreError::InvalidInput("chunk range overflow".into()))?;
            let slice = raw_bytes.get(start..end).ok_or_else(|| {
                StoreError::InvalidInput(format!(
                    "chunk range {start}..{end} exceeds record length {}",
                    raw_bytes.len()
                ))
            })?;
            let actual_sha = sha256_hex(slice);
            if actual_sha != chunk.sha256 {
                return Err(StoreError::IntegrityMismatch(format!(
                    "chunk sha256 mismatch for {document_id}: expected {}, got {actual_sha}",
                    chunk.sha256
                )));
            }
            let (scale, quantized) = quantize(&chunk.vector);
            pieces.push(Piece {
                offset: doc.raw.offset + chunk.offset,
                length: chunk.length,
                sha256: chunk.sha256.clone(),
                text_sha256: chunk.text_sha256.clone(),
                scale,
                vector: quantized,
            });
        }
        let dense_doc = Document {
            id: doc.id,
            agent_id: doc.metadata.agent_id,
            source: doc
                .metadata
                .source
                .as_source()
                .unwrap_or("transcripts")
                .to_owned(),
            timestamp_ms: doc.metadata.timestamp_ms,
            raw_sha256: doc.raw.sha256,
        };
        let dense =
            Self::open_or_get_dense(dense_path, &mut state.dense_spaces, vector_space, dims)?;
        dense.put(&dense_doc, &pieces, &[])?;
        Ok(())
    }
}

fn lookup_document_unfiltered(
    host: &HostIndex,
    document_id: &str,
) -> Result<Option<crate::tokenize::IndexHit>, StoreError> {
    Ok(host.document_unfiltered(document_id)?)
}

#[async_trait]
impl MemoryStore for SqliteStore {
    fn backend_name(&self) -> &'static str {
        "sqlite"
    }

    async fn migrate(&self) -> Result<(), StoreError> {
        // Schema is initialized atomically on `SqliteStore::open`.
        drop(self.lock_state()?);
        Ok(())
    }

    async fn append_record(&self, record: &RawRecordInput) -> Result<RawCitation, StoreError> {
        self.append_record_sync(record)
    }

    async fn upsert_embeddings(
        &self,
        document_id: &str,
        vector_space: &str,
        chunks: &[EmbeddingChunk],
    ) -> Result<(), StoreError> {
        self.upsert_embeddings_sync(document_id, vector_space, chunks)
    }

    async fn search_lexical(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<SearchHit>, StoreError> {
        if request.owners.is_empty() {
            return Ok(Vec::new());
        }
        let state = self.lock_state()?;
        let hits = state
            .host
            .search(&request.owners, request.scope, &request.query)?;
        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            let bytes = read_raw(&hit.window)?;
            let rel_offset = hit.window.offset.saturating_sub(hit.raw.offset);
            // SQLite FTS5 bm25() returns more negative values for better matches; invert to positive score.
            let score = -hit.lexical_score;
            out.push(SearchHit {
                document_id: hit.id.clone(),
                metadata: hit.metadata.clone(),
                citation: RawCitation {
                    document_id: hit.id,
                    agent_id: hit.metadata.agent_id,
                    offset: rel_offset,
                    length: hit.window.length,
                    sha256: hit.window.sha256,
                    raw_sha256: hit.raw.sha256,
                    seq: hit.raw.seq,
                },
                raw_slice: bytes,
                lexical_score: Some(score),
                dense_score: None,
                score,
            });
        }
        Ok(out)
    }

    async fn search_dense(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<SearchHit>, StoreError> {
        if request.owners.is_empty() {
            return Ok(Vec::new());
        }
        let Some(query_vec) = request.query_vector.as_ref().filter(|v| !v.is_empty()) else {
            return Ok(Vec::new());
        };
        let space = request.effective_vector_space();
        let dims = query_vec.len();
        let dense_path = self.dense_file_path(space, dims);
        if !dense_path.exists() {
            return Ok(Vec::new());
        }
        let mut state = self.lock_state()?;
        let dense_hits = {
            let dense = Self::open_or_get_dense(dense_path, &mut state.dense_spaces, space, dims)?;
            dense.search(
                query_vec,
                &request.owners,
                request.scope.as_source(),
                request.query.since_ms,
                request.query.until_ms,
                request.effective_limit(),
            )?
        };
        if dense_hits.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = dense_hits.iter().map(|h| h.document_id.clone()).collect();
        let allowed: HashMap<String, crate::tokenize::IndexHit> = state
            .host
            .filter_documents(&request.owners, request.scope, &request.query, &ids)?
            .into_iter()
            .map(|h| (h.id.clone(), h))
            .collect();
        let mut out = Vec::new();
        for hit in dense_hits {
            let Some(doc) = allowed.get(&hit.document_id) else {
                continue;
            };
            let slice_coord = RawCoordinate {
                segment: doc.raw.segment.clone(),
                offset: hit.offset,
                length: hit.length,
                sha256: hit.sha256.clone(),
                seq: doc.raw.seq,
            };
            let Ok(bytes) = read_raw(&slice_coord) else {
                continue;
            };
            let rel_offset = hit.offset.saturating_sub(doc.raw.offset);
            out.push(SearchHit {
                document_id: doc.id.clone(),
                metadata: doc.metadata.clone(),
                citation: RawCitation {
                    document_id: doc.id.clone(),
                    agent_id: doc.metadata.agent_id.clone(),
                    offset: rel_offset,
                    length: hit.length,
                    sha256: hit.sha256,
                    raw_sha256: doc.raw.sha256.clone(),
                    seq: doc.raw.seq,
                },
                raw_slice: bytes,
                lexical_score: None,
                dense_score: Some(hit.score),
                score: f64::from(hit.score),
            });
        }
        Ok(out)
    }

    async fn read_citation(
        &self,
        owners: &[String],
        document_id: &str,
        offset: u64,
        length: u64,
        expected_sha256: Option<&str>,
    ) -> Result<Option<(RawCitation, Vec<u8>)>, StoreError> {
        if owners.is_empty() {
            return Ok(None);
        }
        if length == 0 || length > MAX_RAW_RANGE_BYTES {
            return Err(StoreError::InvalidInput(format!(
                "citation length must be in 1..={MAX_RAW_RANGE_BYTES}"
            )));
        }
        let state = self.lock_state()?;
        let Some(doc) = state.host.document(owners, MemoryScope::All, document_id)? else {
            return Ok(None);
        };
        let (window, bytes) = read_raw_range(&doc.raw, offset, length).map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidData {
                StoreError::IntegrityMismatch(e.to_string())
            } else if e.kind() == std::io::ErrorKind::InvalidInput {
                StoreError::InvalidInput(e.to_string())
            } else {
                StoreError::Io(e)
            }
        })?;
        if let Some(expected) = expected_sha256
            && window.sha256 != expected
        {
            return Err(StoreError::IntegrityMismatch(format!(
                "citation sha256 mismatch: expected {expected}, got {}",
                window.sha256
            )));
        }
        let citation = RawCitation {
            document_id: doc.id,
            agent_id: doc.metadata.agent_id,
            offset,
            length: window.length,
            sha256: window.sha256,
            raw_sha256: doc.raw.sha256,
            seq: doc.raw.seq,
        };
        Ok(Some((citation, bytes)))
    }

    async fn delete_record(
        &self,
        owners: &[String],
        document_id: &str,
    ) -> Result<bool, StoreError> {
        if owners.is_empty() {
            return Ok(false);
        }
        let mut state = self.lock_state()?;
        let deleted = state.host.delete(owners, document_id)?;
        if deleted {
            for dense in state.dense_spaces.values() {
                let _ = dense.forget(&[document_id.to_owned()]);
            }
        }
        Ok(deleted)
    }

    async fn stats(&self) -> Result<StoreStats, StoreError> {
        let state = self.lock_state()?;
        let documents = state.host.document_count()?;
        let mut vectors = 0u64;
        for dense in state.dense_spaces.values() {
            vectors = vectors.saturating_add(dense.counts()?.1);
        }
        Ok(StoreStats { documents, vectors })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenize::HistorySearchQuery;

    fn rec(id: &str, agent: &str, ts: i64, payload: &[u8]) -> RawRecordInput {
        RawRecordInput {
            id: id.into(),
            agent_id: agent.into(),
            source: MemoryScope::Transcripts,
            timestamp_ms: Some(ts),
            direction: "in".into(),
            sender: "user:1".into(),
            destination: format!("agent:{agent}"),
            role: Some("user".into()),
            tool_name: None,
            tool_names: Vec::new(),
            raw_bytes: payload.to_vec(),
        }
    }

    #[tokio::test]
    async fn sqlite_store_end_to_end_raw_rag_hybrid_rrf_acl_and_exact_citations() {
        let dir = crate::test_tempdir();
        let store = SqliteStore::open(dir.path()).unwrap();
        store.migrate().await.unwrap();
        assert_eq!(store.backend_name(), "sqlite");

        let raw0 = br#"{"content":"deploy failed on staging"}"#;
        let raw1 = "{\"content\":\"部署失敗需要 rollback\"}".as_bytes();
        let raw2 = br#"{"content":"deploy notes for friday"}"#;
        let raw_peer = br#"{"content":"deploy secret for peer"}"#;

        let c0 = store
            .append_record(&rec("d0", "agt_1", 100, raw0))
            .await
            .unwrap();
        let c1 = store
            .append_record(&rec("d1", "agt_1", 200, raw1))
            .await
            .unwrap();
        let c2 = store
            .append_record(&rec("d2", "agt_1", 300, raw2))
            .await
            .unwrap();
        store
            .append_record(&rec("peer0", "agt_2", 400, raw_peer))
            .await
            .unwrap();

        assert_eq!(c0.sha256, sha256_hex(raw0));
        assert_eq!(c1.length, raw1.len() as u64);

        let chunk = |raw: &[u8], vec: Vec<f32>| EmbeddingChunk {
            piece_index: 0,
            offset: 0,
            length: raw.len() as u64,
            sha256: sha256_hex(raw),
            text_sha256: sha256_hex(raw),
            vector: vec,
        };
        store
            .upsert_embeddings("d0", "space-v1", &[chunk(raw0, vec![0.6, 0.8])])
            .await
            .unwrap();
        store
            .upsert_embeddings("d1", "space-v1", &[chunk(raw1, vec![1.0, 0.0])])
            .await
            .unwrap();
        store
            .upsert_embeddings("d2", "space-v1", &[chunk(raw2, vec![0.8, 0.6])])
            .await
            .unwrap();
        store
            .upsert_embeddings("peer0", "space-v1", &[chunk(raw_peer, vec![1.0, 0.0])])
            .await
            .unwrap();

        // Tampered embedding slice SHA must fail closed.
        let bad_chunk = EmbeddingChunk {
            sha256: "0".repeat(64),
            ..chunk(raw0, vec![1.0, 0.0])
        };
        assert!(matches!(
            store
                .upsert_embeddings("d0", "space-v1", &[bad_chunk])
                .await,
            Err(StoreError::IntegrityMismatch(_))
        ));

        // Lexical BM25 search isolates owners and finds English deploy records.
        let lex = store
            .search_lexical(&MemorySearchRequest {
                owners: vec!["agt_1".into()],
                scope: MemoryScope::Transcripts,
                query: HistorySearchQuery {
                    query: Some("deploy".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
        let lex_ids: Vec<&str> = lex.iter().map(|h| h.document_id.as_str()).collect();
        assert_eq!(lex_ids.len(), 2);
        assert!(lex_ids.contains(&"d0") && lex_ids.contains(&"d2"));

        // Dense search ranks Cantonese d1 first while excluding peer0.
        let dense = store
            .search_dense(&MemorySearchRequest {
                owners: vec!["agt_1".into()],
                scope: MemoryScope::Transcripts,
                query: HistorySearchQuery::default(),
                query_vector: Some(vec![1.0, 0.0]),
                vector_space: Some("space-v1".into()),
            })
            .await
            .unwrap();
        assert_eq!(dense[0].document_id, "d1");
        assert!(dense.iter().all(|h| h.citation.agent_id == "agt_1"));

        // Hybrid RRF fuses both lists cleanly.
        let hybrid = store
            .search_hybrid(&MemorySearchRequest {
                owners: vec!["agt_1".into()],
                scope: MemoryScope::Transcripts,
                query: HistorySearchQuery {
                    query: Some("deploy".into()),
                    ..Default::default()
                },
                query_vector: Some(vec![1.0, 0.0]),
                vector_space: Some("space-v1".into()),
            })
            .await
            .unwrap();
        assert_eq!(hybrid.len(), 3);
        // d2 ranks well in both lexical and dense.
        assert_eq!(hybrid[0].document_id, "d2");
        assert!(hybrid[0].lexical_score.is_some());
        assert!(hybrid[0].dense_score.is_some());

        // Exact raw-byte slice citation readback + integrity verification.
        let expected_slice = &raw2[12..24];
        let expected_sha = sha256_hex(expected_slice);
        let (cit, slice) = store
            .read_citation(&["agt_1".into()], "d2", 12, 12, Some(&expected_sha))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slice, expected_slice);
        assert_eq!(cit.raw_sha256, c2.raw_sha256);

        // Wrong expected SHA fails closed.
        assert!(matches!(
            store
                .read_citation(&["agt_1".into()], "d2", 12, 12, Some("bad"))
                .await,
            Err(StoreError::IntegrityMismatch(_))
        ));

        // Unauthorized owner cannot read or delete d2.
        assert!(
            store
                .read_citation(&["agt_2".into()], "d2", 0, 10, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!store.delete_record(&["agt_2".into()], "d2").await.unwrap());

        // Authorized delete removes document and its dense vectors.
        assert!(store.delete_record(&["agt_1".into()], "d2").await.unwrap());
        let stats = store.stats().await.unwrap();
        assert_eq!(stats.documents, 3);
        assert_eq!(stats.vectors, 3);

        // Persistence across reopen.
        drop(store);
        let reopened = SqliteStore::open(dir.path()).unwrap();
        let (_, bytes) = reopened
            .read_citation(
                &["agt_1".into()],
                "d0",
                0,
                raw0.len() as u64,
                Some(&c0.sha256),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, raw0);
    }
}
