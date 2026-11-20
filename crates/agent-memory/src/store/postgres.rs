//! Production PostgreSQL + `pgvector` implementation of [`MemoryStore`].
//!
//! Stores immutable raw history bytes in `memory_records` alongside a stored
//! generated `TSVECTOR` column (GIN-indexed) and dense embeddings in
//! `memory_embeddings` (`pgvector`). Supports single-roundtrip SQL CTE
//! Reciprocal Rank Fusion, strict owner ACL filtering (`agent_id = ANY($1)`),
//! and SHA-256-authenticated exact raw-byte citation readback.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};

use crate::{
    fusion::K as RRF_K,
    raw::{MAX_RAW_RANGE_BYTES, sha256_hex},
    store::{
        EmbeddingChunk, MemorySearchRequest, MemoryStore, RawCitation, RawRecordInput, SearchHit,
        StoreError, StoreStats,
    },
    text::readable,
    tokenize::{
        HistorySort, IndexMetadata, MemoryScope, SearchTerm, tokenize_hybrid_search_tokens,
        tokenize_search_terms,
    },
};

/// Idempotent PostgreSQL + `pgvector` schema migration SQL for [`PostgresPgvectorStore`].
pub const POSTGRES_SCHEMA_SQL: &str = r#"
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS memory_records (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('memory', 'transcripts')),
    timestamp_ms BIGINT,
    direction TEXT NOT NULL,
    sender TEXT NOT NULL,
    destination TEXT NOT NULL,
    role TEXT,
    tool_name TEXT,
    tool_names JSONB NOT NULL DEFAULT '[]'::jsonb,
    raw_bytes BYTEA NOT NULL,
    raw_sha256 TEXT NOT NULL,
    raw_length BIGINT NOT NULL CHECK (raw_length >= 0),
    seq BIGSERIAL NOT NULL,
    search_text TEXT NOT NULL,
    search_tsv TSVECTOR GENERATED ALWAYS AS (to_tsvector('simple', search_text)) STORED
);

CREATE INDEX IF NOT EXISTS memory_records_owner_idx
    ON memory_records (agent_id, source, timestamp_ms DESC, seq DESC);

CREATE INDEX IF NOT EXISTS memory_records_tsv_gin_idx
    ON memory_records USING GIN (search_tsv);

CREATE TABLE IF NOT EXISTS memory_embeddings (
    document_id TEXT NOT NULL REFERENCES memory_records(id) ON DELETE CASCADE,
    vector_space TEXT NOT NULL,
    piece_index INTEGER NOT NULL CHECK (piece_index >= 0),
    byte_offset BIGINT NOT NULL CHECK (byte_offset >= 0),
    byte_length BIGINT NOT NULL CHECK (byte_length >= 0),
    slice_sha256 TEXT NOT NULL,
    text_sha256 TEXT NOT NULL,
    embedding VECTOR NOT NULL,
    PRIMARY KEY (document_id, vector_space, piece_index)
);

CREATE INDEX IF NOT EXISTS memory_embeddings_space_idx
    ON memory_embeddings (vector_space, document_id);

CREATE INDEX IF NOT EXISTS memory_embeddings_text_sha_idx
    ON memory_embeddings (vector_space, text_sha256);
"#;

/// Individual DDL statements executed in order by [`PostgresPgvectorStore::migrate`].
pub fn migration_statements() -> Vec<&'static str> {
    POSTGRES_SCHEMA_SQL
        .split(';')
        .map(str::trim)
        .filter(|stmt| !stmt.is_empty())
        .collect()
}

/// Optional HNSW index DDL helper for a fixed-dimension `vector_space`.
/// Callers with a known embedding width (e.g., 768 or 1024) can execute this
/// expression index for sub-linear ANN recall on large multi-tenant tables.
pub fn hnsw_index_sql(
    index_name: &str,
    vector_space: &str,
    dims: usize,
) -> Result<String, StoreError> {
    if index_name.is_empty()
        || !index_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(StoreError::InvalidInput(
            "HNSW index name must be non-empty ASCII alphanumeric/underscore".into(),
        ));
    }
    if dims == 0 || dims > 16_000 {
        return Err(StoreError::InvalidInput(
            "HNSW vector dimensions must be in 1..=16000".into(),
        ));
    }
    let escaped_space = vector_space.replace('\'', "''");
    Ok(format!(
        "CREATE INDEX IF NOT EXISTS {index_name} \
         ON memory_embeddings USING hnsw ((embedding::vector({dims})) vector_cosine_ops) \
         WHERE vector_space = '{escaped_space}'"
    ))
}

/// Format a slice of finite `f32` values into a `pgvector` text literal (`[0.1,0.2,...]`).
pub fn format_pgvector(values: &[f32]) -> Result<String, StoreError> {
    if values.is_empty() {
        return Err(StoreError::InvalidInput(
            "pgvector literal requires at least one dimension".into(),
        ));
    }
    let mut out = String::with_capacity(values.len() * 10 + 2);
    out.push('[');
    for (i, &val) in values.iter().enumerate() {
        if !val.is_finite() {
            return Err(StoreError::InvalidInput(format!(
                "vector element {i} is not finite: {val}"
            )));
        }
        if i > 0 {
            out.push(',');
        }
        use std::fmt::Write as _;
        let _ = write!(out, "{val}");
    }
    out.push(']');
    Ok(out)
}

/// Parse a `pgvector` text literal (`[0.1,0.2,...]`) back into `Vec<f32>`.
pub fn parse_pgvector(literal: &str) -> Result<Vec<f32>, StoreError> {
    let trimmed = literal.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| StoreError::InvalidInput(format!("invalid pgvector literal: {literal}")))?;
    if inner.trim().is_empty() {
        return Err(StoreError::InvalidInput(
            "empty pgvector literal is invalid".into(),
        ));
    }
    let mut out = Vec::new();
    for part in inner.split(',') {
        let v: f32 = part.trim().parse().map_err(|e| {
            StoreError::InvalidInput(format!("invalid pgvector float '{part}': {e}"))
        })?;
        if !v.is_finite() {
            return Err(StoreError::InvalidInput(format!(
                "non-finite pgvector float '{part}'"
            )));
        }
        out.push(v);
    }
    Ok(out)
}

/// Convert raw query strings into an OR-combined PostgreSQL tsquery, preserving
/// adjacency within each multi-character CJK term.
pub fn build_tsquery_expression(
    query: &Option<String>,
    extra_queries: &[String],
) -> Option<String> {
    fn quote_lexeme(token: &str) -> String {
        format!("'{}'", token.replace('\\', "\\\\").replace('\'', "''"))
    }

    let mut terms: Vec<String> = query
        .iter()
        .chain(extra_queries.iter())
        .flat_map(|q| tokenize_search_terms(q))
        .map(|term| match term {
            SearchTerm::Token(token) => quote_lexeme(&token),
            SearchTerm::CjkPhrase(bigrams) => bigrams
                .iter()
                .map(|token| quote_lexeme(token))
                .collect::<Vec<_>>()
                .join(" <-> "),
        })
        .collect();
    // <-> binds more tightly than |, so the phrase is one OR alternative.
    terms.sort_unstable();
    terms.dedup();
    (!terms.is_empty()).then(|| terms.join(" | "))
}

/// Build the normalized lexical search text indexed into `memory_records.search_text`.
pub fn build_search_text(raw_bytes: &[u8]) -> String {
    let readable_text = readable(raw_bytes);
    let mut tokens = tokenize_hybrid_search_tokens(&readable_text);
    let raw_lossy = String::from_utf8_lossy(raw_bytes);
    tokens.extend(tokenize_hybrid_search_tokens(&raw_lossy));
    tokens.join(" ")
}

/// Typed bind value produced by the SQL query builders (allows full unit testing without live PG).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SqlBind {
    Text(String),
    TextArray(Vec<String>),
    Int64(i64),
    Float64(f64),
}

/// Parameterized PostgreSQL statement built for lexical, dense, or hybrid RRF queries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuiltQuery {
    pub sql: String,
    pub binds: Vec<SqlBind>,
}

fn append_common_filters(
    request: &MemorySearchRequest,
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlBind>,
    alias: &str,
) {
    // Parameter $1 is always `owners: TEXT[]`.
    let push_bind = |binds: &mut Vec<SqlBind>, val: SqlBind| -> usize {
        binds.push(val);
        binds.len()
    };

    if let Some(agent) = request.query.agent.as_deref().filter(|a| *a != "all") {
        let idx = push_bind(binds, SqlBind::Text(agent.to_owned()));
        where_clauses.push(format!("{alias}.agent_id = ${idx}"));
    }
    if let Some(source) = request.scope.as_source() {
        let idx = push_bind(binds, SqlBind::Text(source.to_owned()));
        where_clauses.push(format!("{alias}.source = ${idx}"));
    }
    if let Some(direction) = request.query.direction.as_deref().filter(|d| *d != "all") {
        let idx = push_bind(binds, SqlBind::Text(direction.to_owned()));
        where_clauses.push(format!("{alias}.direction = ${idx}"));
    }
    if let Some(sender) = &request.query.sender {
        let idx = push_bind(binds, SqlBind::Text(format!("%{}%", sender.trim())));
        where_clauses.push(format!("{alias}.sender ILIKE ${idx}"));
    }
    if let Some(peer) = &request.query.with_peer {
        let peer_trimmed = peer.trim();
        let addr = if peer_trimmed.contains(':') {
            peer_trimmed.to_owned()
        } else {
            format!("agent:{peer_trimmed}")
        };
        let p1 = push_bind(binds, SqlBind::Text(peer_trimmed.to_owned()));
        let p2 = push_bind(binds, SqlBind::Text(addr));
        where_clauses.push(format!(
            "({alias}.sender = ${p1} OR {alias}.destination = ${p1} OR {alias}.sender = ${p2} OR {alias}.destination = ${p2})"
        ));
    }
    if request.query.only_tools {
        where_clauses.push(format!(
            "({alias}.tool_name IS NOT NULL OR jsonb_array_length({alias}.tool_names) > 0)"
        ));
    }
    if !request.query.include_tools
        && !request.query.only_tools
        && request.query.tool_name.is_none()
    {
        where_clauses.push(format!(
            "({alias}.tool_name IS NULL AND jsonb_array_length({alias}.tool_names) = 0)"
        ));
    }
    if let Some(tool) = &request.query.tool_name {
        let idx = push_bind(binds, SqlBind::Text(tool.trim().to_ascii_lowercase()));
        where_clauses.push(format!(
            "(lower({alias}.tool_name) = ${idx} OR EXISTS (SELECT 1 FROM jsonb_array_elements_text({alias}.tool_names) t(val) WHERE lower(t.val) = ${idx}))"
        ));
    }
    if let Some(since_ms) = request.query.since_ms {
        let idx = push_bind(binds, SqlBind::Int64(since_ms));
        where_clauses.push(format!("{alias}.timestamp_ms > ${idx}"));
    }
    if let Some(until_ms) = request.query.until_ms {
        let idx = push_bind(binds, SqlBind::Int64(until_ms));
        where_clauses.push(format!("{alias}.timestamp_ms <= ${idx}"));
    }
    if let Some(from_seq) = request.query.from_seq {
        let idx = push_bind(binds, SqlBind::Int64(from_seq));
        where_clauses.push(format!("{alias}.seq >= ${idx}"));
    }
    if let Some(to_seq) = request.query.to_seq {
        let idx = push_bind(binds, SqlBind::Int64(to_seq));
        where_clauses.push(format!("{alias}.seq <= ${idx}"));
    }
}

/// Build the parameterized PostgreSQL lexical (`tsvector` / `ts_rank_cd`) query.
/// Returns `None` if `request.owners` is empty (fail-closed ACL).
pub fn build_lexical_query(request: &MemorySearchRequest) -> Option<BuiltQuery> {
    if request.owners.is_empty() {
        return None;
    }
    let mut binds = vec![SqlBind::TextArray(request.owners.clone())];
    let mut where_clauses = vec!["r.agent_id = ANY($1)".to_owned()];
    append_common_filters(request, &mut where_clauses, &mut binds, "r");

    let tsquery = build_tsquery_expression(&request.query.query, &request.query.queries);
    let score_expr = if let Some(expr) = tsquery {
        binds.push(SqlBind::Text(expr));
        let idx = binds.len();
        where_clauses.push(format!("r.search_tsv @@ to_tsquery('simple', ${idx})"));
        format!("ts_rank_cd(r.search_tsv, to_tsquery('simple', ${idx}))::double precision")
    } else {
        "0.0::double precision".to_owned()
    };

    let order_by = match request.query.sort {
        HistorySort::Oldest => "r.timestamp_ms ASC NULLS LAST, r.seq ASC, r.id ASC",
        HistorySort::Newest => "r.timestamp_ms DESC NULLS LAST, r.seq DESC, r.id DESC",
        HistorySort::Relevance => "lexical_score DESC, r.timestamp_ms DESC NULLS LAST, r.seq DESC",
    };

    binds.push(SqlBind::Int64(request.effective_limit() as i64));
    let limit_idx = binds.len();
    let max_slice = MAX_RAW_RANGE_BYTES as i64;
    let sql = format!(
        "SELECT r.id, r.agent_id, r.source, r.timestamp_ms, r.direction, r.sender, r.destination, \
                r.role, r.tool_name, r.tool_names, r.raw_sha256, r.raw_length, r.seq, \
                0::bigint AS slice_offset, \
                LEAST(r.raw_length, {max_slice})::bigint AS slice_length, \
                substring(r.raw_bytes FROM 1 FOR {max_slice}) AS raw_slice, \
                {score_expr} AS lexical_score \
         FROM memory_records r \
         WHERE {} \
         ORDER BY {order_by} \
         LIMIT ${limit_idx}",
        where_clauses.join(" AND ")
    );
    Some(BuiltQuery { sql, binds })
}

/// Build the parameterized PostgreSQL `pgvector` cosine similarity query.
/// Returns `None` if `owners` or `query_vector` is empty.
pub fn build_dense_query(request: &MemorySearchRequest) -> Result<Option<BuiltQuery>, StoreError> {
    if request.owners.is_empty() {
        return Ok(None);
    }
    let Some(query_vec) = request.query_vector.as_ref().filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let vec_lit = format_pgvector(query_vec)?;
    let mut binds = vec![
        SqlBind::TextArray(request.owners.clone()),
        SqlBind::Text(request.effective_vector_space().to_owned()),
        SqlBind::Text(vec_lit),
    ];
    let mut where_clauses = vec![
        "r.agent_id = ANY($1)".to_owned(),
        "e.vector_space = $2".to_owned(),
    ];
    append_common_filters(request, &mut where_clauses, &mut binds, "r");
    binds.push(SqlBind::Int64(request.effective_limit() as i64));
    let limit_idx = binds.len();

    let sql = format!(
        "WITH best_piece AS ( \
             SELECT DISTINCT ON (r.id) \
                 r.id, r.agent_id, r.source, r.timestamp_ms, r.direction, r.sender, r.destination, \
                 r.role, r.tool_name, r.tool_names, r.raw_sha256, r.raw_length, r.seq, \
                 e.byte_offset, e.byte_length, e.slice_sha256, \
                 substring(r.raw_bytes FROM (e.byte_offset + 1)::integer FOR e.byte_length::integer) AS raw_slice, \
                 (1.0 - (e.embedding <=> $3::vector))::real AS dense_score \
             FROM memory_records r \
             JOIN memory_embeddings e ON e.document_id = r.id \
             WHERE {} \
             ORDER BY r.id, (e.embedding <=> $3::vector) ASC \
         ) \
         SELECT id, agent_id, source, timestamp_ms, direction, sender, destination, \
                role, tool_name, tool_names, raw_sha256, raw_length, seq, \
                byte_offset, byte_length, slice_sha256, raw_slice, dense_score \
         FROM best_piece \
         ORDER BY dense_score DESC, id ASC \
         LIMIT ${limit_idx}",
        where_clauses.join(" AND ")
    );
    Ok(Some(BuiltQuery { sql, binds }))
}

/// Build the single-roundtrip PostgreSQL CTE query combining `tsvector` BM25/rank
/// and `pgvector` cosine similarity via Reciprocal Rank Fusion (`1 / (60 + rank)`).
pub fn build_hybrid_rrf_query(
    request: &MemorySearchRequest,
) -> Result<Option<BuiltQuery>, StoreError> {
    if request.owners.is_empty() {
        return Ok(None);
    }
    let Some(query_vec) = request.query_vector.as_ref().filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let Some(tsquery) = build_tsquery_expression(&request.query.query, &request.query.queries)
    else {
        return Ok(None);
    };
    let vec_lit = format_pgvector(query_vec)?;
    let limit = request.effective_limit();
    let candidate_limit = (limit.saturating_mul(3)).clamp(limit, 200) as i64;

    let mut binds = vec![
        SqlBind::TextArray(request.owners.clone()),
        SqlBind::Text(request.effective_vector_space().to_owned()),
        SqlBind::Text(vec_lit),
        SqlBind::Text(tsquery),
        SqlBind::Float64(RRF_K),
        SqlBind::Int64(candidate_limit),
    ];
    let mut where_clauses = vec!["r.agent_id = ANY($1)".to_owned()];
    append_common_filters(request, &mut where_clauses, &mut binds, "r");
    binds.push(SqlBind::Int64(limit as i64));
    let limit_idx = binds.len();
    let base_where = where_clauses.join(" AND ");
    let max_slice = MAX_RAW_RANGE_BYTES as i64;

    let sql = format!(
        "WITH lexical AS ( \
             SELECT r.id, \
                    ts_rank_cd(r.search_tsv, to_tsquery('simple', $4))::double precision AS lex_score, \
                    ROW_NUMBER() OVER (ORDER BY ts_rank_cd(r.search_tsv, to_tsquery('simple', $4)) DESC, r.timestamp_ms DESC NULLS LAST, r.seq DESC) AS lex_rank \
             FROM memory_records r \
             WHERE {base_where} AND r.search_tsv @@ to_tsquery('simple', $4) \
             LIMIT $6 \
         ), \
         dense_best AS ( \
             SELECT DISTINCT ON (r.id) \
                    r.id, e.byte_offset, e.byte_length, e.slice_sha256, \
                    (1.0 - (e.embedding <=> $3::vector))::real AS dense_score \
             FROM memory_records r \
             JOIN memory_embeddings e ON e.document_id = r.id \
             WHERE {base_where} AND e.vector_space = $2 \
             ORDER BY r.id, (e.embedding <=> $3::vector) ASC \
         ), \
         dense AS ( \
             SELECT id, byte_offset, byte_length, slice_sha256, dense_score, \
                    ROW_NUMBER() OVER (ORDER BY dense_score DESC, id ASC) AS dense_rank \
             FROM dense_best \
             LIMIT $6 \
         ), \
         fused AS ( \
             SELECT COALESCE(l.id, d.id) AS id, \
                    l.lex_score, \
                    d.dense_score, \
                    d.byte_offset, \
                    d.byte_length, \
                    d.slice_sha256, \
                    (COALESCE(1.0 / ($5 + l.lex_rank), 0.0) + COALESCE(1.0 / ($5 + d.dense_rank), 0.0))::double precision AS rrf_score \
             FROM lexical l \
             FULL OUTER JOIN dense d ON d.id = l.id \
         ) \
         SELECT r.id, r.agent_id, r.source, r.timestamp_ms, r.direction, r.sender, r.destination, \
                r.role, r.tool_name, r.tool_names, r.raw_sha256, r.raw_length, r.seq, \
                COALESCE(f.byte_offset, 0::bigint) AS slice_offset, \
                COALESCE(f.byte_length, LEAST(r.raw_length, {max_slice})::bigint) AS slice_length, \
                f.slice_sha256, \
                substring(r.raw_bytes FROM (COALESCE(f.byte_offset, 0) + 1)::integer FOR COALESCE(f.byte_length, LEAST(r.raw_length, {max_slice}))::integer) AS raw_slice, \
                f.lex_score, f.dense_score, f.rrf_score \
         FROM fused f \
         JOIN memory_records r ON r.id = f.id \
         ORDER BY f.rrf_score DESC, r.id ASC \
         LIMIT ${limit_idx}"
    );
    Ok(Some(BuiltQuery { sql, binds }))
}

fn bind_all<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    binds: &'q [SqlBind],
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for bind in binds {
        q = match bind {
            SqlBind::Text(v) => q.bind(v.as_str()),
            SqlBind::TextArray(v) => q.bind(v),
            SqlBind::Int64(v) => q.bind(*v),
            SqlBind::Float64(v) => q.bind(*v),
        };
    }
    q
}

/// PostgreSQL + `pgvector` [`MemoryStore`] backed by a [`sqlx::PgPool`].
#[derive(Clone, Debug)]
pub struct PostgresPgvectorStore {
    pool: PgPool,
}

impl PostgresPgvectorStore {
    /// Wrap an existing [`sqlx::PgPool`].
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Connect to a PostgreSQL instance using `database_url`.
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    /// Borrow the underlying [`sqlx::PgPool`].
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn decode_row_metadata(row: &sqlx::postgres::PgRow) -> Result<IndexMetadata, StoreError> {
    let source_str: String = row.try_get("source")?;
    let tool_names_val: serde_json::Value = row.try_get("tool_names")?;
    let tool_names: Vec<String> = serde_json::from_value(tool_names_val)?;
    Ok(IndexMetadata {
        agent_id: row.try_get("agent_id")?,
        source: MemoryScope::from_source(&source_str),
        timestamp_ms: row.try_get("timestamp_ms")?,
        direction: row.try_get("direction")?,
        sender: row.try_get("sender")?,
        destination: row.try_get("destination")?,
        tool_name: row.try_get("tool_name")?,
        tool_names,
        role: row.try_get("role")?,
    })
}

fn has_excluded_bytes(slice: &[u8], exclude: &[String]) -> bool {
    if exclude.is_empty() {
        return false;
    }
    let lower = String::from_utf8_lossy(slice).to_ascii_lowercase();
    exclude
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
}

#[async_trait]
impl MemoryStore for PostgresPgvectorStore {
    fn backend_name(&self) -> &'static str {
        "postgres-pgvector"
    }

    async fn migrate(&self) -> Result<(), StoreError> {
        for stmt in migration_statements() {
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        Ok(())
    }

    async fn append_record(&self, record: &RawRecordInput) -> Result<RawCitation, StoreError> {
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
        let source = record.source.as_source().ok_or_else(|| {
            StoreError::InvalidInput("record source must be Memory or Transcripts".into())
        })?;
        let raw_sha256 = sha256_hex(&record.raw_bytes);
        let raw_length = record.raw_bytes.len() as i64;
        let search_text = build_search_text(&record.raw_bytes);
        let tool_names = serde_json::to_value(&record.tool_names)?;

        let row = sqlx::query(
            "INSERT INTO memory_records (
                id, agent_id, source, timestamp_ms, direction, sender, destination,
                role, tool_name, tool_names, raw_bytes, raw_sha256, raw_length, search_text
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
             ON CONFLICT (id) DO UPDATE SET
                timestamp_ms = EXCLUDED.timestamp_ms,
                direction = EXCLUDED.direction,
                sender = EXCLUDED.sender,
                destination = EXCLUDED.destination,
                role = EXCLUDED.role,
                tool_name = EXCLUDED.tool_name,
                tool_names = EXCLUDED.tool_names,
                raw_bytes = EXCLUDED.raw_bytes,
                raw_sha256 = EXCLUDED.raw_sha256,
                raw_length = EXCLUDED.raw_length,
                search_text = EXCLUDED.search_text
             WHERE memory_records.agent_id = EXCLUDED.agent_id
             RETURNING seq",
        )
        .bind(&record.id)
        .bind(&record.agent_id)
        .bind(source)
        .bind(record.timestamp_ms)
        .bind(&record.direction)
        .bind(&record.sender)
        .bind(&record.destination)
        .bind(&record.role)
        .bind(&record.tool_name)
        .bind(tool_names)
        .bind(&record.raw_bytes)
        .bind(&raw_sha256)
        .bind(raw_length)
        .bind(&search_text)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            StoreError::IntegrityMismatch(format!(
                "record {} already exists under a different owner",
                record.id
            ))
        })?;

        let seq: i64 = row.try_get("seq")?;
        Ok(RawCitation {
            document_id: record.id.clone(),
            agent_id: record.agent_id.clone(),
            offset: 0,
            length: record.raw_bytes.len() as u64,
            sha256: raw_sha256.clone(),
            raw_sha256,
            seq: seq as u64,
        })
    }

    async fn upsert_embeddings(
        &self,
        document_id: &str,
        vector_space: &str,
        chunks: &[EmbeddingChunk],
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let raw_row = sqlx::query("SELECT raw_bytes FROM memory_records WHERE id = $1")
            .bind(document_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("document {document_id} not found")))?;
        let raw_bytes: Vec<u8> = raw_row.try_get("raw_bytes")?;

        sqlx::query("DELETE FROM memory_embeddings WHERE document_id = $1 AND vector_space = $2")
            .bind(document_id)
            .bind(vector_space)
            .execute(&mut *tx)
            .await?;

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
            let vec_lit = format_pgvector(&chunk.vector)?;
            sqlx::query(
                "INSERT INTO memory_embeddings (
                    document_id, vector_space, piece_index, byte_offset, byte_length,
                    slice_sha256, text_sha256, embedding
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::vector)",
            )
            .bind(document_id)
            .bind(vector_space)
            .bind(chunk.piece_index as i32)
            .bind(chunk.offset as i64)
            .bind(chunk.length as i64)
            .bind(&chunk.sha256)
            .bind(&chunk.text_sha256)
            .bind(vec_lit)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn search_lexical(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let Some(built) = build_lexical_query(request) else {
            return Ok(Vec::new());
        };
        // SAFETY: `built.sql` is assembled solely from static SQL clauses and
        // positional `$N` placeholders; all caller values are bound via `built.binds`.
        let rows = bind_all(
            sqlx::query(sqlx::AssertSqlSafe(built.sql.as_str())),
            &built.binds,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut hits = Vec::with_capacity(rows.len());
        for row in rows {
            let raw_slice: Vec<u8> = row.try_get("raw_slice")?;
            if has_excluded_bytes(&raw_slice, &request.query.exclude) {
                continue;
            }
            let metadata = decode_row_metadata(&row)?;
            let id: String = row.try_get("id")?;
            let raw_sha256: String = row.try_get("raw_sha256")?;
            let seq: i64 = row.try_get("seq")?;
            let slice_offset: i64 = row.try_get("slice_offset")?;
            let slice_length: i64 = row.try_get("slice_length")?;
            let lexical_score: f64 = row.try_get("lexical_score")?;
            let slice_sha256 = sha256_hex(&raw_slice);
            hits.push(SearchHit {
                document_id: id.clone(),
                metadata: metadata.clone(),
                citation: RawCitation {
                    document_id: id,
                    agent_id: metadata.agent_id,
                    offset: slice_offset as u64,
                    length: slice_length as u64,
                    sha256: slice_sha256,
                    raw_sha256,
                    seq: seq as u64,
                },
                raw_slice,
                lexical_score: Some(lexical_score),
                dense_score: None,
                score: lexical_score,
            });
        }
        Ok(hits)
    }

    async fn search_dense(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let Some(built) = build_dense_query(request)? else {
            return Ok(Vec::new());
        };
        // SAFETY: `built.sql` is assembled solely from static SQL clauses and
        // positional `$N` placeholders; all caller values are bound via `built.binds`.
        let rows = bind_all(
            sqlx::query(sqlx::AssertSqlSafe(built.sql.as_str())),
            &built.binds,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut hits = Vec::with_capacity(rows.len());
        for row in rows {
            let raw_slice: Vec<u8> = row.try_get("raw_slice")?;
            let expected_slice_sha: String = row.try_get("slice_sha256")?;
            let actual_slice_sha = sha256_hex(&raw_slice);
            if actual_slice_sha != expected_slice_sha {
                return Err(StoreError::IntegrityMismatch(format!(
                    "dense slice digest mismatch: expected {expected_slice_sha}, got {actual_slice_sha}"
                )));
            }
            if has_excluded_bytes(&raw_slice, &request.query.exclude) {
                continue;
            }
            let metadata = decode_row_metadata(&row)?;
            let id: String = row.try_get("id")?;
            let raw_sha256: String = row.try_get("raw_sha256")?;
            let seq: i64 = row.try_get("seq")?;
            let byte_offset: i64 = row.try_get("byte_offset")?;
            let byte_length: i64 = row.try_get("byte_length")?;
            let dense_score: f32 = row.try_get("dense_score")?;
            hits.push(SearchHit {
                document_id: id.clone(),
                metadata: metadata.clone(),
                citation: RawCitation {
                    document_id: id,
                    agent_id: metadata.agent_id,
                    offset: byte_offset as u64,
                    length: byte_length as u64,
                    sha256: actual_slice_sha,
                    raw_sha256,
                    seq: seq as u64,
                },
                raw_slice,
                lexical_score: None,
                dense_score: Some(dense_score),
                score: f64::from(dense_score),
            });
        }
        Ok(hits)
    }

    async fn search_hybrid(
        &self,
        request: &MemorySearchRequest,
    ) -> Result<Vec<SearchHit>, StoreError> {
        if request.owners.is_empty() {
            return Ok(Vec::new());
        }
        let Some(built) = build_hybrid_rrf_query(request)? else {
            return if request.query_vector.as_ref().is_some_and(|v| !v.is_empty()) {
                self.search_dense(request).await
            } else {
                self.search_lexical(request).await
            };
        };
        // SAFETY: `built.sql` is assembled solely from static SQL clauses and
        // positional `$N` placeholders; all caller values are bound via `built.binds`.
        let rows = bind_all(
            sqlx::query(sqlx::AssertSqlSafe(built.sql.as_str())),
            &built.binds,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut hits = Vec::with_capacity(rows.len());
        for row in rows {
            let raw_slice: Vec<u8> = row.try_get("raw_slice")?;
            let expected_slice_sha: Option<String> = row.try_get("slice_sha256")?;
            let actual_slice_sha = sha256_hex(&raw_slice);
            if let Some(expected) = expected_slice_sha
                && actual_slice_sha != expected
            {
                return Err(StoreError::IntegrityMismatch(format!(
                    "hybrid slice digest mismatch: expected {expected}, got {actual_slice_sha}"
                )));
            }
            if has_excluded_bytes(&raw_slice, &request.query.exclude) {
                continue;
            }
            let metadata = decode_row_metadata(&row)?;
            let id: String = row.try_get("id")?;
            let raw_sha256: String = row.try_get("raw_sha256")?;
            let seq: i64 = row.try_get("seq")?;
            let slice_offset: i64 = row.try_get("slice_offset")?;
            let slice_length: i64 = row.try_get("slice_length")?;
            let lexical_score: Option<f64> = row.try_get("lex_score")?;
            let dense_score: Option<f32> = row.try_get("dense_score")?;
            let rrf_score: f64 = row.try_get("rrf_score")?;
            hits.push(SearchHit {
                document_id: id.clone(),
                metadata: metadata.clone(),
                citation: RawCitation {
                    document_id: id,
                    agent_id: metadata.agent_id,
                    offset: slice_offset as u64,
                    length: slice_length as u64,
                    sha256: actual_slice_sha,
                    raw_sha256,
                    seq: seq as u64,
                },
                raw_slice,
                lexical_score,
                dense_score,
                score: rrf_score,
            });
        }
        Ok(hits)
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
        let pg_start = i32::try_from(offset.saturating_add(1))
            .map_err(|_| StoreError::InvalidInput("citation offset exceeds i32 range".into()))?;
        let pg_len = i32::try_from(length)
            .map_err(|_| StoreError::InvalidInput("citation length exceeds i32 range".into()))?;

        let row = sqlx::query(
            "SELECT id, agent_id, raw_sha256, raw_length, seq, \
                    substring(raw_bytes FROM $3 FOR $4) AS slice \
             FROM memory_records \
             WHERE id = $1 AND agent_id = ANY($2)",
        )
        .bind(document_id)
        .bind(owners)
        .bind(pg_start)
        .bind(pg_len)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let raw_length: i64 = row.try_get("raw_length")?;
        if offset > raw_length as u64 {
            return Err(StoreError::InvalidInput(format!(
                "citation offset {offset} exceeds record length {raw_length}"
            )));
        }
        let slice: Vec<u8> = row.try_get("slice")?;
        let actual_sha = sha256_hex(&slice);
        if let Some(expected) = expected_sha256
            && actual_sha != expected
        {
            return Err(StoreError::IntegrityMismatch(format!(
                "citation sha256 mismatch: expected {expected}, got {actual_sha}"
            )));
        }
        let citation = RawCitation {
            document_id: row.try_get("id")?,
            agent_id: row.try_get("agent_id")?,
            offset,
            length: slice.len() as u64,
            sha256: actual_sha,
            raw_sha256: row.try_get("raw_sha256")?,
            seq: row.try_get::<i64, _>("seq")? as u64,
        };
        Ok(Some((citation, slice)))
    }

    async fn delete_record(
        &self,
        owners: &[String],
        document_id: &str,
    ) -> Result<bool, StoreError> {
        if owners.is_empty() {
            return Ok(false);
        }
        let result = sqlx::query("DELETE FROM memory_records WHERE id = $1 AND agent_id = ANY($2)")
            .bind(document_id)
            .bind(owners)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn stats(&self) -> Result<StoreStats, StoreError> {
        let row = sqlx::query(
            "SELECT (SELECT count(*) FROM memory_records) AS docs, \
                    (SELECT count(*) FROM memory_embeddings) AS vecs",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(StoreStats {
            documents: row.try_get::<i64, _>("docs")? as u64,
            vectors: row.try_get::<i64, _>("vecs")? as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenize::HistorySearchQuery;

    #[test]
    fn schema_migration_is_idempotent_and_defines_pgvector_and_gin_indexes() {
        let stmts = migration_statements();
        assert!(stmts.len() >= 6, "expected extension, tables, and indexes");
        assert!(stmts[0].contains("CREATE EXTENSION IF NOT EXISTS vector"));
        assert!(
            stmts
                .iter()
                .any(|s| s.contains("CREATE TABLE IF NOT EXISTS memory_records")
                    && s.contains("raw_bytes BYTEA NOT NULL")
                    && s.contains(
                        "GENERATED ALWAYS AS (to_tsvector('simple', search_text)) STORED"
                    ))
        );
        assert!(stmts.iter().any(
            |s| s.contains("CREATE TABLE IF NOT EXISTS memory_embeddings")
                && s.contains("ON DELETE CASCADE")
                && s.contains("embedding VECTOR NOT NULL")
        ));
        assert!(stmts.iter().any(|s| s.contains("USING GIN (search_tsv)")));

        let hnsw = hnsw_index_sql("idx_emb_768", "granite-768", 768).unwrap();
        assert!(hnsw.contains("USING hnsw ((embedding::vector(768)) vector_cosine_ops)"));
        assert!(hnsw_index_sql("bad-name!", "space", 768).is_err());
        assert!(hnsw_index_sql("ok_name", "space", 0).is_err());
    }

    #[test]
    fn pgvector_formatting_and_parsing_roundtrip_and_reject_non_finite() {
        let vec = vec![0.25_f32, -0.5, 0.75, 0.0];
        let literal = format_pgvector(&vec).unwrap();
        assert_eq!(literal, "[0.25,-0.5,0.75,0]");
        assert_eq!(parse_pgvector(&literal).unwrap(), vec);

        assert!(format_pgvector(&[]).is_err());
        assert!(format_pgvector(&[f32::NAN]).is_err());
        assert!(format_pgvector(&[f32::INFINITY]).is_err());
        assert!(parse_pgvector("not-a-vector").is_err());
        assert!(parse_pgvector("[]").is_err());
    }

    #[test]
    fn lexical_dense_and_hybrid_rrf_sql_builders_enforce_acl_and_parameterization() {
        // Empty owners must always fail closed.
        let empty = MemorySearchRequest::default();
        assert!(build_lexical_query(&empty).is_none());
        assert!(build_dense_query(&empty).unwrap().is_none());
        assert!(build_hybrid_rrf_query(&empty).unwrap().is_none());

        let req = MemorySearchRequest {
            owners: vec!["agt_1".into(), "agt_2".into()],
            scope: MemoryScope::Transcripts,
            query: HistorySearchQuery {
                query: Some("deploy_staging 部署".into()),
                since_ms: Some(1000),
                until_ms: Some(2000),
                tool_name: Some("workspace".into()),
                limit: 15,
                ..Default::default()
            },
            query_vector: Some(vec![0.6, 0.8]),
            vector_space: Some("embed-v1".into()),
        };

        let lex = build_lexical_query(&req).unwrap();
        assert!(lex.sql.contains("r.agent_id = ANY($1)"));
        assert!(lex.sql.contains("ts_rank_cd(r.search_tsv"));
        assert!(lex.sql.contains("substring(r.raw_bytes FROM 1 FOR"));
        assert!(lex.binds.contains(&SqlBind::Text("transcripts".into())));
        assert!(lex.binds.contains(&SqlBind::Int64(1000)));
        assert!(lex.binds.contains(&SqlBind::Int64(2000)));
        assert!(lex.binds.contains(&SqlBind::Text("workspace".into())));

        let dense = build_dense_query(&req).unwrap().unwrap();
        assert!(dense.sql.contains("e.embedding <=> $3::vector"));
        assert!(dense.sql.contains("e.vector_space = $2"));
        assert!(
            dense
                .sql
                .contains("substring(r.raw_bytes FROM (e.byte_offset + 1)::integer")
        );
        assert_eq!(dense.binds[1], SqlBind::Text("embed-v1".into()));
        assert_eq!(dense.binds[2], SqlBind::Text("[0.6,0.8]".into()));

        let hybrid = build_hybrid_rrf_query(&req).unwrap().unwrap();
        assert!(hybrid.sql.contains("FULL OUTER JOIN dense"));
        assert!(hybrid.sql.contains("1.0 / ($5 + l.lex_rank)"));
        assert!(hybrid.sql.contains("1.0 / ($5 + d.dense_rank)"));
        assert!(hybrid.binds.contains(&SqlBind::Float64(RRF_K)));

        // Round-trip serialization of BuiltQuery and SearchHit request types.
        let json = serde_json::to_string(&hybrid).unwrap();
        let back: BuiltQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(back, hybrid);
    }

    #[test]
    fn tsquery_builder_sanitizes_operators_and_preserves_cjk_bigrams() {
        let expr = build_tsquery_expression(
            &Some("foo'bar & baz | 部署！頭像圖Deploy".into()),
            &["頭像圖 頭".into()],
        );
        assert_eq!(
            expr.as_deref(),
            Some("'bar' | 'baz' | 'deploy' | 'foo' | '部署' | '頭' | '頭像' <-> '像圖'")
        );
        assert_eq!(build_tsquery_expression(&None, &["' | !".into()]), None);
    }

    /// Live PostgreSQL + pgvector end-to-end integration test.
    /// Automatically skipped unless `AGENT_MEMORY_TEST_POSTGRES_URL` is set.
    #[tokio::test]
    async fn live_postgres_pgvector_end_to_end_when_configured() {
        let Ok(url) = std::env::var("AGENT_MEMORY_TEST_POSTGRES_URL") else {
            return;
        };
        let store = PostgresPgvectorStore::connect(&url)
            .await
            .expect("connect test postgres");
        store.migrate().await.expect("run migrations");

        let id = format!("test-pg-{}", uuid::Uuid::now_v7());
        let raw = br#"{"content":"staging deploy rollback guide"}"#.to_vec();
        let citation = store
            .append_record(&RawRecordInput {
                id: id.clone(),
                agent_id: "agt_pg_1".into(),
                source: MemoryScope::Transcripts,
                timestamp_ms: Some(1_700_000_000_000),
                direction: "in".into(),
                sender: "user:1".into(),
                destination: "agent:agt_pg_1".into(),
                role: Some("user".into()),
                tool_name: None,
                tool_names: Vec::new(),
                raw_bytes: raw.clone(),
            })
            .await
            .expect("append record");
        assert_eq!(citation.raw_sha256, sha256_hex(&raw));

        store
            .upsert_embeddings(
                &id,
                "test-space",
                &[EmbeddingChunk {
                    piece_index: 0,
                    offset: 0,
                    length: raw.len() as u64,
                    sha256: sha256_hex(&raw),
                    text_sha256: sha256_hex(b"staging deploy rollback guide"),
                    vector: vec![1.0, 0.0, 0.0],
                }],
            )
            .await
            .expect("upsert embeddings");

        let req = MemorySearchRequest {
            owners: vec!["agt_pg_1".into()],
            scope: MemoryScope::Transcripts,
            query: HistorySearchQuery {
                query: Some("rollback".into()),
                ..Default::default()
            },
            query_vector: Some(vec![1.0, 0.0, 0.0]),
            vector_space: Some("test-space".into()),
        };
        let hybrid = store.search_hybrid(&req).await.expect("hybrid search");
        assert!(hybrid.iter().any(|h| h.document_id == id));

        // Exact raw-byte readback verifies SHA-256 and ACL.
        let (read_cit, bytes) = store
            .read_citation(
                &["agt_pg_1".into()],
                &id,
                0,
                raw.len() as u64,
                Some(&citation.sha256),
            )
            .await
            .expect("read citation")
            .expect("citation found");
        assert_eq!(bytes, raw);
        assert_eq!(read_cit.sha256, citation.sha256);

        // Unauthorized owner sees nothing.
        assert!(
            store
                .read_citation(&["agt_other".into()], &id, 0, 10, None)
                .await
                .unwrap()
                .is_none()
        );

        assert!(
            store
                .delete_record(&["agt_pg_1".into()], &id)
                .await
                .unwrap()
        );
    }
}
