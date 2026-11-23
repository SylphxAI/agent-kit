//! Rebuildable SQLite host index: metadata and contentless FTS5 BM25 windows only.
//!
//! Raw bytes remain in their immutable segment files. Every search and lookup
//! requires an explicit caller-authorized owner set; an empty owner set returns
//! zero results.

use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, params_from_iter,
    types::Value,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::Path,
};

use crate::{
    raw::{RawCoordinate, RawSource, read_raw, sha256_hex},
    tokenize::{
        HistorySearchQuery, HistorySort, IndexHit, IndexMetadata, MemoryScope, SearchTerm,
        tokenize_hybrid_search_tokens, tokenize_search_terms,
    },
};

/// Default filename for a SQLite host history index inside a data directory.
pub const INDEX_FILE: &str = "host-history-index-v1.db";

const WINDOW_BYTES: usize = 4096;
const OVERLAP_BYTES: usize = 256;

/// Errors returned by [`HostIndex`] operations.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

fn decode_tool_names(value: String) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

/// Contentless SQLite FTS5 BM25 index over immutable raw history coordinates.
pub struct HostIndex {
    conn: Connection,
}

impl HostIndex {
    /// Open or initialize a SQLite FTS5 host index at `path`.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        match Self::open_strict(path) {
            Err(IndexError::Sql(error))
                if error.to_string().contains("malformed")
                    || error.to_string().contains("not a database") =>
            {
                let _ = fs::remove_file(path);
                let _ = fs::remove_file(path.with_extension("db-wal"));
                let _ = fs::remove_file(path.with_extension("db-shm"));
                Self::open_strict(path)
            }
            other => other,
        }
    }

    pub(crate) fn open_strict(path: &Path) -> Result<Self, IndexError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON; PRAGMA synchronous=NORMAL;",
        )?;
        let already_initialized: bool = conn
            .query_row(
                "SELECT count(*)=2 FROM sqlite_master WHERE type='table' AND name IN ('host_fts','host_documents')",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if already_initialized {
            Self::ensure_generation(&conn)?;
            return Ok(Self { conn });
        }
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let init = (|| -> Result<(), IndexError> {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS host_documents (
                    id TEXT PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    source TEXT NOT NULL,
                    timestamp_ms INTEGER,
                    direction TEXT NOT NULL,
                    sender TEXT NOT NULL,
                    destination TEXT NOT NULL,
                    tool_name TEXT,
                    role TEXT,
                    segment TEXT NOT NULL,
                    offset INTEGER NOT NULL,
                    length INTEGER NOT NULL,
                    sha256 TEXT NOT NULL,
                    seq INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS host_document_tools (
                    document_id TEXT NOT NULL REFERENCES host_documents(id) ON DELETE CASCADE,
                    name TEXT NOT NULL,
                    PRIMARY KEY(document_id, name)
                );
                CREATE INDEX IF NOT EXISTS host_documents_owner
                    ON host_documents(agent_id, source, timestamp_ms);
                CREATE TABLE IF NOT EXISTS host_windows (
                    id INTEGER PRIMARY KEY,
                    document_id TEXT NOT NULL REFERENCES host_documents(id) ON DELETE CASCADE,
                    offset INTEGER NOT NULL,
                    length INTEGER NOT NULL,
                    sha256 TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS host_windows_document ON host_windows(document_id);
                CREATE VIRTUAL TABLE IF NOT EXISTS host_fts
                    USING fts5(tokens, content='', contentless_delete=1);
                CREATE TRIGGER IF NOT EXISTS host_windows_delete
                    AFTER DELETE ON host_windows BEGIN
                        DELETE FROM host_fts WHERE rowid=old.id;
                    END;
                CREATE TABLE IF NOT EXISTS host_generation (
                    id INTEGER PRIMARY KEY CHECK(id=1),
                    generation TEXT NOT NULL
                );",
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO host_generation VALUES(1, ?1)",
                [uuid::Uuid::now_v7().to_string()],
            )?;
            conn.execute(
                "INSERT INTO host_fts(host_fts, rank) VALUES('secure-delete', 1)",
                [],
            )?;
            Ok(())
        })();
        match init {
            Ok(()) => {
                conn.execute_batch("COMMIT;")?;
                Ok(Self { conn })
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK;");
                Err(error)
            }
        }
    }

    fn ensure_generation(conn: &Connection) -> Result<(), IndexError> {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='host_generation')",
            [],
            |r| r.get(0),
        )?;
        if present {
            return Ok(());
        }
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS host_generation(id INTEGER PRIMARY KEY CHECK(id=1), generation TEXT NOT NULL);
             INSERT OR IGNORE INTO host_generation VALUES(1, '{}');",
            uuid::Uuid::now_v7()
        ))?;
        Ok(())
    }

    /// Generation UUID created when the documents table was initialized.
    pub fn generation(&self) -> Result<Option<String>, IndexError> {
        Ok(self
            .conn
            .query_row(
                "SELECT generation FROM host_generation WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Count total indexed documents.
    pub fn document_count(&self) -> Result<u64, IndexError> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM host_documents", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Idempotently index a verified raw coordinate in 4 KiB overlapping FTS5 windows.
    pub fn index(
        &mut self,
        id: &str,
        metadata: &IndexMetadata,
        raw: &RawCoordinate,
    ) -> Result<bool, IndexError> {
        let mut source = RawSource::open(&raw.segment)?;
        source.verify(raw)?;
        if self.is_current(id, metadata, raw)? {
            return Ok(false);
        }
        let prepared = prepare_document(&mut source, id, metadata, raw)?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM host_documents WHERE id=?1", [id])?;
        Self::insert_prepared(&tx, id, &prepared)?;
        tx.commit()?;
        Ok(true)
    }

    /// Reserve the database writer before checking the global document ID.
    pub(crate) fn begin_write(&mut self) -> Result<Transaction<'_>, IndexError> {
        Ok(self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    /// Insert a fresh document without replacing an existing ID.
    pub(crate) fn insert_new(
        tx: &Transaction<'_>,
        id: &str,
        metadata: &IndexMetadata,
        raw: &RawCoordinate,
    ) -> Result<(), IndexError> {
        let mut source = RawSource::open(&raw.segment)?;
        source.verify(raw)?;
        let prepared = prepare_document(&mut source, id, metadata, raw)?;
        Self::insert_prepared(tx, id, &prepared)
    }

    fn insert_prepared(
        tx: &Transaction<'_>,
        id: &str,
        prepared: &PreparedDocument,
    ) -> Result<(), IndexError> {
        let source_name = prepared
            .metadata
            .source
            .as_source()
            .ok_or_else(|| io::Error::other("an indexed document needs one source"))?;
        tx.execute(
            "INSERT INTO host_documents VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                id,
                prepared.metadata.agent_id,
                source_name,
                prepared.metadata.timestamp_ms,
                prepared.metadata.direction,
                prepared.metadata.sender,
                prepared.metadata.destination,
                prepared.metadata.tool_name,
                prepared.metadata.role,
                prepared.raw.segment.to_string_lossy(),
                prepared.raw.offset,
                prepared.raw.length,
                prepared.raw.sha256,
                prepared.raw.seq,
            ],
        )?;
        for name in tool_set(&prepared.metadata) {
            tx.execute(
                "INSERT INTO host_document_tools VALUES(?1,?2)",
                params![id, name],
            )?;
        }
        for window in &prepared.windows {
            tx.execute(
                "INSERT INTO host_windows(document_id,offset,length,sha256) VALUES(?1,?2,?3,?4)",
                params![id, window.offset, window.length, window.sha256],
            )?;
            let row = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO host_fts(rowid,tokens) VALUES(?1,?2)",
                params![row, window.tokens],
            )?;
        }
        Ok(())
    }

    /// Delete one document within `owners` ACL. Returns `true` if deleted.
    pub fn delete(&mut self, owners: &[String], id: &str) -> Result<bool, IndexError> {
        if owners.is_empty() {
            return Ok(false);
        }
        let mut values: Vec<Value> = owners.iter().cloned().map(Value::Text).collect();
        let placeholders = vec!["?"; values.len()].join(",");
        values.push(id.to_owned().into());
        let sql = format!("DELETE FROM host_documents WHERE agent_id IN ({placeholders}) AND id=?");
        let changed = self.conn.execute(&sql, params_from_iter(values))?;
        Ok(changed > 0)
    }

    fn is_current(
        &self,
        id: &str,
        metadata: &IndexMetadata,
        raw: &RawCoordinate,
    ) -> Result<bool, IndexError> {
        let source = metadata
            .source
            .as_source()
            .ok_or_else(|| io::Error::other("an indexed document needs one source"))?;
        let old: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT json_array(segment,offset,length,sha256,seq),
                        json_array(agent_id,source,timestamp_ms,direction,sender,destination,tool_name,role),
                        agent_id
                 FROM host_documents WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((raw_identity, meta_identity, owner)) = old else {
            return Ok(false);
        };
        if owner != metadata.agent_id {
            return Err(io::Error::other("indexed document owner cannot change").into());
        }
        Ok(raw_identity == raw_json(raw)
            && meta_identity == meta_json(metadata, source)
            && self.document_tools(id)? == tool_set(metadata))
    }

    fn document_tools(&self, document: &str) -> Result<Vec<String>, rusqlite::Error> {
        self.conn
            .prepare("SELECT name FROM host_document_tools WHERE document_id=?1 ORDER BY name")?
            .query_map([document], |r| r.get(0))?
            .collect()
    }

    /// Internal lookup by global ID for append admission and embedding indexing.
    pub(crate) fn document_unfiltered(&self, id: &str) -> Result<Option<IndexHit>, IndexError> {
        Self::document_by_id(&self.conn, id)
    }

    pub(crate) fn document_by_id(
        conn: &Connection,
        id: &str,
    ) -> Result<Option<IndexHit>, IndexError> {
        let sql = "SELECT id,agent_id,source,timestamp_ms,direction,sender,destination,tool_name,role,
                segment,offset,length,sha256,seq,
                (SELECT json_group_array(name) FROM (SELECT name FROM host_document_tools WHERE document_id=d.id ORDER BY name))
             FROM host_documents d WHERE id=?1";
        Ok(conn
            .query_row(sql, [id], |r| {
                let raw = RawCoordinate {
                    segment: r.get::<_, String>(9)?.into(),
                    offset: r.get(10)?,
                    length: r.get(11)?,
                    sha256: r.get(12)?,
                    seq: r.get(13)?,
                };
                Ok(IndexHit {
                    id: r.get(0)?,
                    metadata: IndexMetadata {
                        agent_id: r.get(1)?,
                        source: MemoryScope::from_source(&r.get::<_, String>(2)?),
                        timestamp_ms: r.get(3)?,
                        direction: r.get(4)?,
                        sender: r.get(5)?,
                        destination: r.get(6)?,
                        tool_name: r.get(7)?,
                        tool_names: decode_tool_names(r.get(14)?)?,
                        role: r.get(8)?,
                    },
                    window: raw.clone(),
                    raw,
                    lexical_score: 0.0,
                })
            })
            .optional()?)
    }

    /// Look up one document strictly within the authorized `visible` owner set.
    pub fn document(
        &self,
        visible: &[String],
        scope: MemoryScope,
        id: &str,
    ) -> Result<Option<IndexHit>, IndexError> {
        if visible.is_empty() {
            return Ok(None);
        }
        let mut values: Vec<Value> = visible.iter().cloned().map(Value::Text).collect();
        let placeholders = vec!["?"; values.len()].join(",");
        values.push(id.to_owned().into());
        let scope_val = scope
            .as_source()
            .map_or(Value::Null, |s| Value::Text(s.into()));
        values.push(scope_val.clone());
        values.push(scope_val);
        let sql = format!(
            "SELECT id,agent_id,source,timestamp_ms,direction,sender,destination,tool_name,role,
                segment,offset,length,sha256,seq,
                (SELECT json_group_array(name) FROM (SELECT name FROM host_document_tools WHERE document_id=d.id ORDER BY name))
             FROM host_documents d
             WHERE agent_id IN ({placeholders}) AND id=? AND (? IS NULL OR source=?)"
        );
        Ok(self
            .conn
            .query_row(&sql, params_from_iter(values), |r| {
                let raw = RawCoordinate {
                    segment: r.get::<_, String>(9)?.into(),
                    offset: r.get(10)?,
                    length: r.get(11)?,
                    sha256: r.get(12)?,
                    seq: r.get(13)?,
                };
                Ok(IndexHit {
                    id: r.get(0)?,
                    metadata: IndexMetadata {
                        agent_id: r.get(1)?,
                        source: MemoryScope::from_source(&r.get::<_, String>(2)?),
                        timestamp_ms: r.get(3)?,
                        direction: r.get(4)?,
                        sender: r.get(5)?,
                        destination: r.get(6)?,
                        tool_name: r.get(7)?,
                        tool_names: decode_tool_names(r.get(14)?)?,
                        role: r.get(8)?,
                    },
                    window: raw.clone(),
                    raw,
                    lexical_score: 0.0,
                })
            })
            .optional()?)
    }

    /// Search within `visible_agents` (empty owner list always fails closed with empty results).
    pub fn search(
        &self,
        visible_agents: &[String],
        scope: MemoryScope,
        query: &HistorySearchQuery,
    ) -> Result<Vec<IndexHit>, IndexError> {
        self.search_in_segment(visible_agents, scope, query, None, None)
    }

    /// Filter a set of candidate IDs (e.g. from dense retrieval) against ACL + structured query filters.
    pub fn filter_documents(
        &self,
        visible_agents: &[String],
        scope: MemoryScope,
        query: &HistorySearchQuery,
        ids: &[String],
    ) -> Result<Vec<IndexHit>, IndexError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut filters = query.clone();
        filters.query = None;
        filters.queries.clear();
        filters.radius = 0;
        filters.sort = HistorySort::Oldest;
        filters.limit = ids.len().min(200);
        self.search_in_segment(visible_agents, scope, &filters, None, Some(ids))
    }

    fn search_in_segment(
        &self,
        visible_agents: &[String],
        scope: MemoryScope,
        query: &HistorySearchQuery,
        segment: Option<&Path>,
        ids: Option<&[String]>,
    ) -> Result<Vec<IndexHit>, IndexError> {
        if visible_agents.is_empty() {
            return Ok(Vec::new());
        }
        let mut values: Vec<Value> = visible_agents.iter().cloned().map(Value::Text).collect();
        let placeholders = vec!["?"; values.len()].join(",");
        let mut where_sql = format!("d.agent_id IN ({placeholders})");
        if let Some(ids) = ids {
            where_sql.push_str(&format!(
                " AND d.id IN ({})",
                vec!["?"; ids.len()].join(",")
            ));
            values.extend(ids.iter().cloned().map(Value::Text));
        }
        let terms: Vec<SearchTerm> = query
            .query
            .iter()
            .chain(query.queries.iter())
            .flat_map(|s| tokenize_search_terms(s))
            .collect();
        let lexical = !terms.is_empty();
        if let Some(segment) = segment {
            let source = segment.to_string_lossy().to_string();
            where_sql.push_str(" AND d.segment=?");
            values.push(source.into());
        }
        let mut push = |clause: &str, value: Value| {
            where_sql.push_str(" AND ");
            where_sql.push_str(clause);
            values.push(value);
        };
        if let Some(agent) = query.agent.as_deref().filter(|a| *a != "all") {
            push("d.agent_id=?", agent.to_owned().into());
        }
        if let Some(source) = scope.as_source() {
            push("d.source=?", source.to_owned().into());
        }
        if let Some(direction) = query.direction.as_deref().filter(|s| *s != "all") {
            push("d.direction=?", direction.to_owned().into());
        }
        if let Some(sender) = &query.sender {
            push(
                "instr(lower(d.sender),lower(?))>0",
                sender.trim().to_owned().into(),
            );
        }
        if let Some(peer) = &query.with_peer {
            let peer = peer.trim();
            let address = if peer.contains(':') {
                peer.to_owned()
            } else {
                format!("agent:{peer}")
            };
            push(
                "(d.sender=? OR d.destination=? OR d.sender=? OR d.destination=?)",
                peer.to_owned().into(),
            );
            values.push(peer.to_owned().into());
            values.push(address.clone().into());
            values.push(address.into());
        }
        if query.only_tools {
            where_sql.push_str(
                " AND (d.tool_name IS NOT NULL OR EXISTS(SELECT 1 FROM host_document_tools t WHERE t.document_id=d.id))",
            );
        }
        if !query.include_tools && !query.only_tools && query.tool_name.is_none() {
            where_sql.push_str(
                " AND d.tool_name IS NULL AND NOT EXISTS(SELECT 1 FROM host_document_tools t WHERE t.document_id=d.id)",
            );
        }
        if let Some(tool) = &query.tool_name {
            where_sql.push_str(
                " AND (lower(d.tool_name)=lower(?) OR EXISTS(SELECT 1 FROM host_document_tools t WHERE t.document_id=d.id AND lower(t.name)=lower(?)))",
            );
            values.push(tool.trim().to_owned().into());
            values.push(tool.trim().to_owned().into());
        }
        for (clause, value) in [
            ("d.timestamp_ms>?", query.since_ms),
            ("d.timestamp_ms<=?", query.until_ms),
            ("d.seq>=?", query.from_seq),
            ("d.seq<=?", query.to_seq),
        ] {
            if let Some(value) = value {
                where_sql.push_str(" AND ");
                where_sql.push_str(clause);
                values.push(value.into());
            }
        }
        if lexical {
            where_sql.push_str(" AND host_fts MATCH ?");
            values.push(
                terms
                    .into_iter()
                    .map(|term| {
                        let phrase = match term {
                            SearchTerm::Token(token) => token,
                            SearchTerm::CjkPhrase(bigrams) => bigrams.join(" "),
                        };
                        format!("\"{}\"", phrase.replace('"', "\"\""))
                    })
                    .collect::<Vec<_>>()
                    .join(" OR ")
                    .into(),
            );
        }
        let tail_default = !lexical && query.sort == HistorySort::Relevance;
        let order = if query.sort == HistorySort::Oldest {
            "d.timestamp_ms ASC,d.seq ASC,d.id ASC"
        } else if tail_default || query.sort == HistorySort::Newest {
            "d.timestamp_ms DESC,d.seq DESC,d.id DESC"
        } else {
            "score ASC,d.timestamp_ms DESC,d.seq DESC"
        };
        let score = if lexical { "bm25(host_fts)" } else { "0.0" };
        let join = if lexical {
            "JOIN host_fts ON host_fts.rowid=w.id"
        } else {
            ""
        };
        let sql = format!(
            "SELECT d.id,d.agent_id,d.source,d.timestamp_ms,d.direction,d.sender,d.destination,d.tool_name,d.role,
                d.segment,d.offset,d.length,d.sha256,d.seq,w.offset,w.length,w.sha256,{score} AS score,
                (SELECT json_group_array(name) FROM (SELECT name FROM host_document_tools WHERE document_id=d.id ORDER BY name))
             FROM host_documents d JOIN host_windows w ON w.document_id=d.id {join}
             WHERE {where_sql} ORDER BY {order}"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(values))?;
        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            if !seen.insert(id.clone()) {
                continue;
            }
            let raw = RawCoordinate {
                segment: row.get::<_, String>(9)?.into(),
                offset: row.get(10)?,
                length: row.get(11)?,
                sha256: row.get(12)?,
                seq: row.get(13)?,
            };
            if has_excluded_text(&raw, &query.exclude)? {
                continue;
            }
            hits.push(IndexHit {
                id,
                metadata: IndexMetadata {
                    agent_id: row.get(1)?,
                    source: MemoryScope::from_source(&row.get::<_, String>(2)?),
                    timestamp_ms: row.get(3)?,
                    direction: row.get(4)?,
                    sender: row.get(5)?,
                    destination: row.get(6)?,
                    tool_name: row.get(7)?,
                    tool_names: decode_tool_names(row.get(18)?)?,
                    role: row.get(8)?,
                },
                window: RawCoordinate {
                    segment: raw.segment.clone(),
                    offset: row.get(14)?,
                    length: row.get(15)?,
                    sha256: row.get(16)?,
                    seq: raw.seq,
                },
                raw,
                lexical_score: row.get(17)?,
            });
            if hits.len() >= query.limit.clamp(1, 200) {
                break;
            }
        }
        if tail_default {
            hits.reverse();
        }
        if query.radius > 0 {
            let mut expanded = hits.clone();
            let mut seen_ids: std::collections::HashSet<_> =
                expanded.iter().map(|h| h.id.clone()).collect();
            for hit in &hits {
                let mut neighbors = query.clone();
                neighbors.query = None;
                neighbors.queries.clear();
                neighbors.radius = 0;
                neighbors.sort = HistorySort::Oldest;
                let radius = query.radius.min(10) as i64;
                let seq = hit.raw.seq as i64;
                neighbors.from_seq =
                    Some(query.from_seq.unwrap_or(0).max(seq.saturating_sub(radius)));
                neighbors.to_seq = Some(
                    query
                        .to_seq
                        .unwrap_or(i64::MAX)
                        .min(seq.saturating_add(radius)),
                );
                neighbors.limit = 21;
                for neighbor in self.search_in_segment(
                    std::slice::from_ref(&hit.metadata.agent_id),
                    scope,
                    &neighbors,
                    Some(&hit.raw.segment),
                    None,
                )? {
                    if neighbor.raw.segment == hit.raw.segment
                        && seen_ids.insert(neighbor.id.clone())
                    {
                        expanded.push(neighbor);
                    }
                }
            }
            expanded.sort_by(|a, b| {
                (a.metadata.timestamp_ms, a.raw.seq, &a.id).cmp(&(
                    b.metadata.timestamp_ms,
                    b.raw.seq,
                    &b.id,
                ))
            });
            return Ok(expanded);
        }
        Ok(hits)
    }
}

fn has_excluded_text(raw: &RawCoordinate, exclude: &[String]) -> Result<bool, IndexError> {
    if exclude.is_empty() {
        return Ok(false);
    }
    let bytes = read_raw(raw)?;
    let lower = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    Ok(exclude
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .any(|needle| lower.contains(&needle.to_ascii_lowercase())))
}

#[derive(Clone, Default)]
struct JsonProjection {
    in_string: bool,
    escaped: bool,
    unicode: Option<(u16, u8)>,
    high: Option<u16>,
}

impl JsonProjection {
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if let Some((value, n)) = self.unicode {
                if let Some(digit) = (b as char).to_digit(16) {
                    let value = (value << 4) | digit as u16;
                    if n == 3 {
                        self.unicode = None;
                        if (0xd800..=0xdbff).contains(&value) {
                            self.high = Some(value);
                        } else {
                            let code = if (0xdc00..=0xdfff).contains(&value) {
                                self.high.take().map(|high| {
                                    0x10000
                                        + (((high as u32 - 0xd800) << 10) | (value as u32 - 0xdc00))
                                })
                            } else {
                                self.high = None;
                                Some(value as u32)
                            };
                            if let Some(ch) = code.and_then(char::from_u32) {
                                let mut buffer = [0; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                            }
                        }
                    } else {
                        self.unicode = Some((value, n + 1));
                    }
                } else {
                    self.unicode = None;
                    out.push(b' ');
                }
                continue;
            }
            if self.escaped {
                self.escaped = false;
                match b {
                    b'u' => self.unicode = Some((0, 0)),
                    b'n' | b'r' | b't' | b'b' | b'f' => out.push(b' '),
                    b'"' | b'\\' | b'/' => out.push(b),
                    _ => out.push(b' '),
                }
                continue;
            }
            if b == b'"' {
                self.in_string = !self.in_string;
                out.push(b' ');
            } else if self.in_string && b == b'\\' {
                self.escaped = true;
            } else if self.in_string {
                out.push(b);
            }
        }
        out
    }
}

#[derive(Clone, Debug)]
struct PreparedWindow {
    offset: u64,
    length: u64,
    sha256: String,
    tokens: String,
}

#[derive(Clone, Debug)]
struct PreparedDocument {
    metadata: IndexMetadata,
    raw: RawCoordinate,
    windows: Vec<PreparedWindow>,
}

fn raw_json(raw: &RawCoordinate) -> String {
    serde_json::json!([raw.segment, raw.offset, raw.length, raw.sha256, raw.seq]).to_string()
}

fn meta_json(metadata: &IndexMetadata, source: &str) -> String {
    serde_json::json!([
        metadata.agent_id,
        source,
        metadata.timestamp_ms,
        metadata.direction,
        metadata.sender,
        metadata.destination,
        metadata.tool_name,
        metadata.role,
    ])
    .to_string()
}

fn tool_set(metadata: &IndexMetadata) -> Vec<String> {
    let mut names = metadata.tool_names.clone();
    names.extend(metadata.tool_name.iter().cloned());
    names.sort();
    names.dedup();
    names
}

fn integrity_mismatch() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "index coordinate integrity mismatch",
    )
}

fn prepare_document(
    file: &mut RawSource,
    _id: &str,
    metadata: &IndexMetadata,
    raw: &RawCoordinate,
) -> Result<PreparedDocument, IndexError> {
    if raw.segment != file.path() {
        return Err(io::Error::other("indexed coordinate names another source").into());
    }
    let mut windows = Vec::new();
    let mut reader = file.range(raw.offset, raw.length);
    let mut hash = Sha256::new();
    let mut carried = 0;
    let mut relative = 0u64;
    let mut json_state = JsonProjection::default();
    let mut bytes = [0u8; WINDOW_BYTES];
    while relative < raw.length {
        let length = (raw.length - relative).min(WINDOW_BYTES as u64) as usize;
        reader
            .read_exact(&mut bytes[carried..length])
            .map_err(|e| match e.kind() {
                io::ErrorKind::UnexpectedEof => integrity_mismatch(),
                _ => e,
            })?;
        hash.update(&bytes[carried..length]);
        let mut tokens = tokenize_hybrid_search_tokens(&String::from_utf8_lossy(&bytes[..length]));
        let mut window_state = json_state.clone();
        let decoded = window_state.feed(&bytes[..length]);
        tokens.extend(tokenize_hybrid_search_tokens(&String::from_utf8_lossy(
            &decoded,
        )));
        let step = if relative + length as u64 == raw.length {
            length
        } else {
            length - OVERLAP_BYTES
        };
        json_state.feed(&bytes[..step]);
        windows.push(PreparedWindow {
            offset: raw.offset + relative,
            length: length as u64,
            sha256: sha256_hex(&bytes[..length]),
            tokens: tokens.join(" "),
        });
        if relative + length as u64 == raw.length {
            break;
        }
        bytes.copy_within(length - OVERLAP_BYTES..length, 0);
        carried = OVERLAP_BYTES;
        relative += (length - OVERLAP_BYTES) as u64;
    }
    if raw.length == 0 {
        windows.push(PreparedWindow {
            offset: raw.offset,
            length: 0,
            sha256: sha256_hex(b""),
            tokens: String::new(),
        });
    }
    if format!("{:x}", hash.finalize()) != raw.sha256 {
        return Err(integrity_mismatch().into());
    }
    Ok(PreparedDocument {
        metadata: metadata.clone(),
        raw: raw.clone(),
        windows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawSegment;

    #[test]
    fn fts5_compound_cjk_queries_rank_exact_hits_and_reject_partial_matches() {
        let dir = crate::test_tempdir();
        let seg = RawSegment::open(&dir.path().join("events.raw")).unwrap();
        let mut index = HostIndex::open(&dir.path().join(INDEX_FILE)).unwrap();
        let avatar = "agent-avatar-style-standard";
        for (id, content, timestamp_ms) in [
            (avatar, "頭像圖 頭像", 10),
            ("other-avatar", "今日頭像更新完成", 20),
            ("single", "由頭跟到尾", 30),
            ("reversed", "像頭", 40),
            ("separated", "頭、像", 50),
            ("separated-pairs", "頭像、像圖", 60),
            ("ascii", "deploy the staging app", 70),
        ] {
            let raw = seg.append(content.as_bytes()).unwrap();
            index
                .index(
                    id,
                    &IndexMetadata {
                        agent_id: "agt_1".into(),
                        source: MemoryScope::Memory,
                        timestamp_ms: Some(timestamp_ms),
                        direction: "out".into(),
                        sender: "agt_1".into(),
                        destination: "agt_1".into(),
                        tool_name: None,
                        tool_names: Vec::new(),
                        role: None,
                    },
                    &raw,
                )
                .unwrap();
        }

        let owners = ["agt_1".to_string()];
        let ids = |query: &str, variants: &[&str]| -> Vec<String> {
            index
                .search(
                    &owners,
                    MemoryScope::Memory,
                    &HistorySearchQuery {
                        query: Some(query.into()),
                        queries: variants.iter().map(|s| (*s).into()).collect(),
                        ..Default::default()
                    },
                )
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect()
        };

        assert_eq!(
            ids("頭像", &[]),
            [avatar, "separated-pairs", "other-avatar"]
        );
        assert_eq!(ids("頭像圖？", &[]), [avatar]);
        assert_eq!(ids("deploy", &[]), ["ascii"]);
        let mut single = ids("頭", &[]);
        single.sort();
        assert_eq!(
            single,
            [
                avatar,
                "other-avatar",
                "reversed",
                "separated",
                "separated-pairs",
                "single"
            ]
        );
        for (query, variants) in [("頭像圖Deploy", &[][..]), ("DEPLOY", &["頭像圖"][..])] {
            let mut hits = ids(query, variants);
            hits.sort();
            assert_eq!(hits, [avatar, "ascii"]);
        }
    }

    #[test]
    fn fts5_bm25_enforces_owner_acl_cjk_search_and_window_citations() {
        let dir = crate::test_tempdir();
        let seg = RawSegment::open(&dir.path().join("events.raw")).unwrap();
        let c1 = seg
            .append(br#"{"content":"deploy staging failed with exit 42"}"#)
            .unwrap();
        let c2 = seg
            .append("{\"content\":\"部署失敗需要 rollback\"}".as_bytes())
            .unwrap();
        let c3 = seg
            .append(br#"{"content":"deploy secret for peer owner"}"#)
            .unwrap();

        let mut index = HostIndex::open(&dir.path().join(INDEX_FILE)).unwrap();
        let meta = |agent: &str, ts: i64| IndexMetadata {
            agent_id: agent.into(),
            source: MemoryScope::Transcripts,
            timestamp_ms: Some(ts),
            direction: "in".into(),
            sender: "user:1".into(),
            destination: format!("agent:{agent}"),
            tool_name: None,
            tool_names: Vec::new(),
            role: Some("user".into()),
        };
        assert!(index.index("d1", &meta("agt_1", 10), &c1).unwrap());
        assert!(!index.index("d1", &meta("agt_1", 10), &c1).unwrap());
        assert!(index.index("d2", &meta("agt_1", 20), &c2).unwrap());
        assert!(index.index("d3", &meta("agt_2", 30), &c3).unwrap());

        // Empty owner set fails closed.
        assert!(
            index
                .search(
                    &[],
                    MemoryScope::All,
                    &HistorySearchQuery {
                        query: Some("deploy".into()),
                        ..Default::default()
                    }
                )
                .unwrap()
                .is_empty()
        );

        // Authorized owner sees only its own record and exact raw bytes verify.
        let hits = index
            .search(
                &["agt_1".into()],
                MemoryScope::Transcripts,
                &HistorySearchQuery {
                    query: Some("deploy".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "d1");
        assert_eq!(
            read_raw(&hits[0].window).unwrap(),
            br#"{"content":"deploy staging failed with exit 42"}"#
        );

        // CJK recall works via hybrid bigram tokenization.
        let cjk = index
            .search(
                &["agt_1".into()],
                MemoryScope::Transcripts,
                &HistorySearchQuery {
                    query: Some("部署".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(cjk.len(), 1);
        assert_eq!(cjk[0].id, "d2");
    }
}
