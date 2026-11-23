//! Embedding and reranking providers: pinned local ONNX models (default, offline),
//! OpenAI-compatible `/embeddings` endpoints, and `/rerank` endpoints (Cohere,
//! Voyage, Jina, OpenRouter).
//!
//! Every embedder stamps its vector space ([`Stamp`]: provider, model, dimension,
//! and preprocessing) so vectors from incompatible spaces are never mixed.

use crate::{
    embed::{Chunk, MAX_PIECES, chunk},
    model::ModelPin,
    runtime::{Encoder, Error},
    text::{is_cjk, project},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Tokens reserved in each piece for special tokens and passage prefix.
const RESERVED_TOKENS: usize = 16;
/// Approximate tokens per piece for a remote embedder, unless configured.
const REMOTE_PIECE_TOKENS: usize = 512;
/// Inputs per remote embedding request.
const REMOTE_BATCH: usize = 64;
/// Retry attempts for throttled or transiently failing upstream requests.
const ATTEMPTS: u32 = 6;
const MAX_WAIT: Duration = Duration::from_secs(60);

/// A remote embedding or reranking model configuration bound to a profile.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// `<provider>/<profile>`, a directory under `<data>/profiles`.
    pub profile: String,
    /// The model ID expected by the remote endpoint.
    pub model: String,
    /// Optional API root (`.../v1`); falls back to the provider's documented root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Embedder: target vector width (Matryoshka truncation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<usize>,
    /// Embedder: approximate tokens per embedded piece.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub piece_tokens: Option<usize>,
    /// Price per million tokens when the endpoint does not report cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_per_million_tokens: Option<f64>,
    /// Reranker: price per search for providers billing per search unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_per_search: Option<f64>,
    /// Reranker: optional character limit on each input (query or passage),
    /// before prefixes. Inputs over the limit are rejected, never clipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passage_chars: Option<usize>,
    /// Optional instruction prefix prepended to queries.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub query_prefix: String,
    /// Optional instruction prefix prepended to passages.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub passage_prefix: String,
    /// Extra JSON fields merged into every request body. OpenRouter requests
    /// always enforce `provider.data_collection = "deny"`.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub body: Map<String, Value>,
    /// Extra JSON fields merged into query requests only.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub query_body: Map<String, Value>,
    /// Extra JSON fields merged into passage requests only.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub passage_body: Map<String, Value>,
}

/// Embedder backend configuration. Defaults to the pinned local ONNX embedder.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "provider", rename_all = "kebab-case")]
pub enum EmbedderConfig {
    /// A pinned local ONNX model (`pins::EMBEDDERS`).
    Local {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// `POST {base_url}/embeddings` (OpenAI-compatible schema).
    #[serde(rename = "openai")]
    OpenAi(Box<Remote>),
}

/// Reranker backend configuration. Defaults to the pinned local ONNX reranker.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "provider", rename_all = "kebab-case")]
pub enum RerankerConfig {
    Local {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// `POST {base_url}/rerank` (Cohere / Voyage / Jina / OpenRouter schema).
    Rerank(Box<Remote>),
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self::Local { model: None }
    }
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self::Local { model: None }
    }
}

/// Service provider configuration (`<data>/memory-service/config.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default)]
    pub embedder: EmbedderConfig,
    #[serde(default)]
    pub reranker: RerankerConfig,
    /// Maximum USD spent rebuilding a new vector space ([`DEFAULT_MAX_REBUILD_USD`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rebuild_usd: Option<f64>,
    /// Maximum USD spent per UTC day outside rebuilds ([`DEFAULT_MAX_DAILY_USD`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_daily_usd: Option<f64>,
}

pub const DEFAULT_MAX_REBUILD_USD: f64 = 10.0;
pub const DEFAULT_MAX_DAILY_USD: f64 = 1.0;

impl ProviderConfig {
    /// Ensure every configured remote model carries an explicit price so spend
    /// caps hold even when upstream endpoints omit `usage.cost`.
    pub fn require_prices(&self) -> Result<(), Error> {
        if let EmbedderConfig::OpenAi(remote) = &self.embedder
            && remote.usd_per_million_tokens.is_none()
        {
            return Err(format!(
                "embedder {} ({}) has no usd_per_million_tokens; spend could not be bounded",
                remote.model, remote.profile
            )
            .into());
        }
        if let RerankerConfig::Rerank(remote) = &self.reranker
            && remote.usd_per_million_tokens.is_none()
            && remote.usd_per_search.is_none()
        {
            return Err(format!(
                "reranker {} ({}) has neither usd_per_million_tokens nor usd_per_search; spend could not be bounded",
                remote.model, remote.profile
            )
            .into());
        }
        Ok(())
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("memory-service").join("config.json")
    }

    /// Load `path`, or return the local default when `path` does not exist.
    pub fn load(path: &Path) -> Result<Self, Error> {
        match std::fs::read(path) {
            Ok(_) => Self::load_required(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display()).into()),
        }
    }

    /// Load an explicit configuration file that must exist.
    pub fn load_required(path: &Path) -> Result<Self, Error> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?)
    }
}

/// Vector space identity: equal stamps produce comparable vectors.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stamp {
    pub provider: String,
    pub model: String,
    pub dimensions: usize,
    /// Preprocessing descriptor: projection, piece budget, token counter, and passage prefix.
    pub preprocessing: String,
}

impl Stamp {
    pub fn key(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Short SHA-256 prefix identifying this space's dense index file.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(self.key().as_bytes()))[..16].to_owned()
    }
}

/// Accumulated token and spend counters for one model instance.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub requests: u64,
    pub tokens: u64,
    /// Reported cost when available, otherwise computed from configured pricing.
    pub usd: f64,
    pub unpriced_requests: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.requests += other.requests;
        self.tokens += other.tokens;
        self.usd += other.usd;
        self.unpriced_requests += other.unpriced_requests;
    }
}

// ---------------------------------------------------------------- profiles

fn default_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "openrouter" => Some("https://openrouter.ai/api/v1"),
        "google-gemini" => Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        "openai" => Some("https://api.openai.com/v1"),
        "voyage" | "voyageai" => Some("https://api.voyageai.com/v1"),
        "cohere" => Some("https://api.cohere.com/v2"),
        "jina" => Some("https://api.jina.ai/v1"),
        _ => None,
    }
}

fn first_file(dir: &Path, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        let key = text.trim();
        (!key.is_empty()).then(|| key.to_owned())
    })
}

/// Read an API key from a neutral profile directory (`api-key`, `<provider>-api-key`,
/// or `.config/<provider>/api-key`).
pub fn profile_api_key(profile_dir: &Path, provider: &str) -> Option<String> {
    let named = format!("{provider}-api-key");
    let config_key = format!(".config/{provider}/api-key");
    let alt = match provider {
        "google-gemini" => first_file(profile_dir, &["gemini-api-key"]),
        _ => None,
    };
    alt.or_else(|| first_file(profile_dir, &["api-key", &named, &config_key]))
}

/// Remote API root and bearer token. Never exposes the key in `Debug` output.
#[derive(Clone)]
pub struct Endpoint {
    pub base_url: String,
    key: String,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    /// Construct an [`Endpoint`] directly from an API root URL and bearer key.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            key: api_key.into(),
        }
    }

    /// Resolve `remote.profile` under `<data_dir>/profiles/<provider>/<profile>`.
    pub fn resolve(data_dir: &Path, remote: &Remote) -> Result<Self, Error> {
        let (provider, profile) = remote
            .profile
            .split_once('/')
            .filter(|(p, n)| !p.is_empty() && !n.is_empty() && !n.contains('/'))
            .ok_or_else(|| format!("profile {:?} is not <provider>/<profile>", remote.profile))?;
        if [provider, profile]
            .iter()
            .any(|part| part.starts_with('.') || part.contains(['\\', '\0']))
        {
            return Err(format!("profile {:?} is not a profile name", remote.profile).into());
        }
        let dir = data_dir.join("profiles").join(provider).join(profile);
        let key = profile_api_key(&dir, provider)
            .ok_or_else(|| format!("profile {} holds no API key", remote.profile))?;
        let base_url = remote
            .base_url
            .clone()
            .or_else(|| default_base_url(provider).map(str::to_owned))
            .ok_or_else(|| {
                format!(
                    "profile {} has no known API root; set base_url",
                    remote.profile
                )
            })?;
        Ok(Self::new(base_url, key))
    }

    /// Offline placeholder endpoint for local token counting without credentials.
    pub fn offline() -> Self {
        Self::new("http://offline.invalid", "")
    }
}

// -------------------------------------------------------------------- HTTP

/// Bounded JSON HTTP client for remote embedding and rerank APIs.
#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_global(Some(Duration::from_secs(120)))
            .http_status_as_error(false)
            .user_agent("agent-memory")
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Http {
    pub fn post(&self, endpoint: &Endpoint, path: &str, body: &Value) -> Result<Value, Error> {
        let url = format!("{}{path}", endpoint.base_url);
        let payload = serde_json::to_vec(body)?;
        let mut wait = Duration::from_secs(2);
        for attempt in 1..=ATTEMPTS {
            let response = self
                .agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", endpoint.key))
                .header("Content-Type", "application/json")
                .send(&payload[..]);
            let mut response = match response {
                Ok(response) => response,
                Err(error) if attempt < ATTEMPTS => {
                    eprintln!("memory-service: {url}: {error}; retrying");
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(MAX_WAIT);
                    continue;
                }
                Err(error) => return Err(format!("{url}: {error}").into()),
            };
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|s| Duration::from_secs(s.min(60)));
            let text = response
                .body_mut()
                .with_config()
                .limit(256 * 1024 * 1024)
                .read_to_string()
                .map_err(|e| format!("{url}: {e}"))?;
            if status == 200 {
                return Ok(serde_json::from_str(&text)
                    .map_err(|e| format!("{url}: unreadable answer: {e}"))?);
            }
            let retryable = matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529);
            if retryable && attempt < ATTEMPTS {
                std::thread::sleep(retry_after.unwrap_or(wait));
                wait = (wait * 2).min(MAX_WAIT);
                continue;
            }
            let summary: String = text.chars().take(300).collect();
            return Err(format!("{url}: HTTP {status}: {summary}").into());
        }
        Err(format!("{url}: no answer").into())
    }
}

fn merged(base: Value, layers: &[&Map<String, Value>]) -> Value {
    let mut base = base;
    if let Value::Object(object) = &mut base {
        for layer in layers {
            for (k, v) in layer.iter() {
                object.insert(k.clone(), v.clone());
            }
        }
    }
    base
}

fn via_openrouter(config: &Remote, endpoint: &Endpoint) -> bool {
    config.profile.split('/').next() == Some("openrouter")
        || endpoint
            .base_url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split(['/', ':']).next())
            .is_some_and(|host| host == "openrouter.ai" || host.ends_with(".openrouter.ai"))
}

fn deny_data_collection(body: &mut Value) {
    let Value::Object(object) = body else {
        return;
    };
    let provider = object
        .entry("provider")
        .or_insert_with(|| Value::Object(Map::new()));
    if !provider.is_object() {
        *provider = Value::Object(Map::new());
    }
    if let Value::Object(provider) = provider {
        provider.insert("data_collection".into(), json!("deny"));
    }
}

fn reported_cost(answer: &Value) -> Option<f64> {
    answer.pointer("/usage/cost").and_then(Value::as_f64)
}

fn normalized(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

// --------------------------------------------------------------- counting

/// Slice `text` up to its `budget`-th approximate token.
pub fn cut_to_tokens(text: &str, budget: usize) -> &str {
    if budget == 0 {
        return "";
    }
    match approximate_tokens(text).get(budget - 1) {
        Some(&(_, end)) => &text[..end],
        None => text,
    }
}

/// Approximate token spans for remote models without a local tokenizer:
/// each CJK character, each punctuation mark, and each run of up to 4 ASCII
/// alphanumerics counts as one token.
pub fn approximate_tokens(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut run: Option<(usize, usize, usize)> = None;
    for (at, c) in text.char_indices() {
        let end = at + c.len_utf8();
        if c.is_alphanumeric() && !is_cjk(c) {
            match &mut run {
                Some((_, e, n)) if *n < 4 => {
                    *e = end;
                    *n += 1;
                }
                _ => {
                    if let Some((s, e, _)) = run.take() {
                        out.push((s, e));
                    }
                    run = Some((at, end, 1));
                }
            }
            continue;
        }
        if let Some((s, e, _)) = run.take() {
            out.push((s, e));
        }
        if !c.is_whitespace() {
            out.push((at, end));
        }
    }
    if let Some((s, e, _)) = run {
        out.push((s, e));
    }
    out
}

// ---------------------------------------------------------------- embedder

/// Stateful remote embedding / reranking HTTP caller with usage tracking.
pub struct RemoteModel {
    pub config: Remote,
    pub endpoint: Endpoint,
    http: Http,
    pub usage: Usage,
    last_request: Option<Instant>,
}

impl RemoteModel {
    pub fn new(config: Remote, endpoint: Endpoint) -> Self {
        Self {
            config,
            endpoint,
            http: Http::default(),
            usage: Usage::default(),
            last_request: None,
        }
    }

    fn pace(&mut self, gap: Duration) {
        if let Some(last) = self.last_request {
            let since = last.elapsed();
            if since < gap {
                std::thread::sleep(gap - since);
            }
        }
        self.last_request = Some(Instant::now());
    }

    fn body(&self, base: Value, extra: &Map<String, Value>) -> Value {
        let mut body = merged(base, &[&self.config.body, extra]);
        if via_openrouter(&self.config, &self.endpoint) {
            deny_data_collection(&mut body);
        }
        body
    }

    fn account(&mut self, answer: &Value, tokens: u64, searches: u64) {
        self.usage.requests += 1;
        self.usage.tokens += tokens;
        match reported_cost(answer) {
            Some(usd) => self.usage.usd += usd,
            None => match (
                self.config.usd_per_search,
                self.config.usd_per_million_tokens,
            ) {
                (Some(per), _) if searches > 0 => self.usage.usd += per * searches as f64,
                (_, Some(per)) => self.usage.usd += per * tokens as f64 / 1e6,
                _ => self.usage.unpriced_requests += 1,
            },
        }
    }

    fn embed(
        &mut self,
        texts: &[String],
        extra: &Map<String, Value>,
    ) -> Result<Vec<Vec<f32>>, Error> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(REMOTE_BATCH) {
            self.pace(Duration::from_millis(200));
            let mut body = json!({
                "model": self.config.model,
                "input": batch,
                "encoding_format": "float",
            });
            if let Some(d) = self.config.dimensions {
                body["dimensions"] = json!(d);
            }
            let body = self.body(body, extra);
            let answer = self.http.post(&self.endpoint, "/embeddings", &body)?;
            let mut rows: Vec<(usize, Vec<f32>)> = answer
                .get("data")
                .and_then(Value::as_array)
                .ok_or("embeddings answer has no data")?
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let index = row
                        .get("index")
                        .and_then(Value::as_u64)
                        .map_or(i, |n| n as usize);
                    let vector: Vec<f32> = row
                        .get("embedding")
                        .and_then(Value::as_array)
                        .map(|v| {
                            v.iter()
                                .filter_map(Value::as_f64)
                                .map(|x| x as f32)
                                .collect()
                        })
                        .unwrap_or_default();
                    (index, vector)
                })
                .collect();
            rows.sort_by_key(|(i, _)| *i);
            if rows.len() != batch.len() || rows.iter().any(|(_, v)| v.is_empty()) {
                return Err(format!(
                    "embeddings answer has {} vectors for {} inputs",
                    rows.len(),
                    batch.len()
                )
                .into());
            }
            let tokens = answer
                .pointer("/usage/prompt_tokens")
                .or_else(|| answer.pointer("/usage/total_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or_else(|| {
                    batch
                        .iter()
                        .map(|t| approximate_tokens(t).len() as u64)
                        .sum()
                });
            self.account(&answer, tokens, 0);
            out.extend(rows.into_iter().map(|(_, v)| normalized(v)));
        }
        Ok(out)
    }

    fn rerank(&mut self, query: &str, passages: &[&str]) -> Result<Vec<f32>, Error> {
        if passages.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(limit) = self.config.passage_chars {
            let check = |label: &str, input: &str| -> Result<(), Error> {
                let chars = input.chars().count();
                if chars > limit {
                    return Err(format!("rerank {label} has {chars} characters, exceeding configured limit {limit}; no request sent").into());
                }
                Ok(())
            };
            check("query", query)?;
            for (index, passage) in passages.iter().enumerate() {
                check(&format!("passage at index {index}"), passage)?;
            }
        }
        for key in ["query", "documents"] {
            if self.config.body.contains_key(key) {
                return Err(format!("rerank body must not override {key}; no request sent").into());
            }
        }
        let documents: Vec<String> = passages
            .iter()
            .map(|p| format!("{}{p}", self.config.passage_prefix))
            .collect();
        let query = format!("{}{query}", self.config.query_prefix);
        let body = json!({
            "model": self.config.model,
            "query": query,
            "documents": documents,
            "top_n": documents.len(),
        });
        let body = self.body(body, &Map::new());
        self.pace(Duration::from_millis(100));
        let answer = self.http.post(&self.endpoint, "/rerank", &body)?;
        let rows = answer
            .get("results")
            .or_else(|| answer.get("data"))
            .and_then(Value::as_array)
            .ok_or("rerank answer has no results")?;
        let mut scores = vec![f32::MIN; passages.len()];
        for row in rows {
            let index = row
                .get("index")
                .and_then(Value::as_u64)
                .ok_or("rerank result without index")? as usize;
            let score = row
                .get("relevance_score")
                .or_else(|| row.get("score"))
                .and_then(Value::as_f64)
                .ok_or("rerank result without relevance_score")?;
            if let Some(slot) = scores.get_mut(index) {
                *slot = score as f32;
            }
        }
        let tokens = answer
            .pointer("/usage/total_tokens")
            .or_else(|| answer.pointer("/meta/billed_units/input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| {
                documents
                    .iter()
                    .map(|p| approximate_tokens(p).len() as u64)
                    .sum::<u64>()
                    + approximate_tokens(&query).len() as u64
            });
        let searches = answer
            .pointer("/usage/search_units")
            .or_else(|| answer.pointer("/meta/billed_units/search_units"))
            .and_then(Value::as_u64)
            .unwrap_or(1);
        self.account(&answer, tokens, searches);
        Ok(scores)
    }
}

/// Parameters for loading a local ONNX model.
pub struct Local<'a> {
    pub environment: &'static ort::environment::Environment,
    pub models: &'a Path,
    pub threads: usize,
}

/// A loaded local or remote embedder.
pub enum Embedder {
    Local(Box<Encoder>),
    Remote {
        model: Box<RemoteModel>,
        dims: usize,
    },
}

/// Resolve the pinned local embedder named by `config`, if local.
pub fn local_embedder_pin(config: &EmbedderConfig) -> Result<Option<ModelPin>, Error> {
    Ok(match config {
        EmbedderConfig::Local { model: None } => Some(crate::pins::EMBEDDER),
        EmbedderConfig::Local { model: Some(name) } => Some(
            crate::pins::by_name(name)
                .filter(|p| p.head != crate::model::Head::Logit)
                .ok_or_else(|| format!("no pinned embedder named {name}"))?,
        ),
        EmbedderConfig::OpenAi(_) => None,
    })
}

/// Resolve the pinned local reranker named by `config`, if local.
pub fn local_reranker_pin(config: &RerankerConfig) -> Result<Option<ModelPin>, Error> {
    Ok(match config {
        RerankerConfig::Local { model: None } => Some(crate::pins::RERANKER),
        RerankerConfig::Local { model: Some(name) } => Some(
            crate::pins::by_name(name)
                .filter(|p| p.head == crate::model::Head::Logit)
                .ok_or_else(|| format!("no pinned reranker named {name}"))?,
        ),
        RerankerConfig::Rerank(_) => None,
    })
}

impl Embedder {
    pub fn load(
        config: &EmbedderConfig,
        data_dir: &Path,
        local: Option<&Local<'_>>,
    ) -> Result<Self, Error> {
        match config {
            EmbedderConfig::Local { .. } => {
                let pin = local_embedder_pin(config)?.ok_or("no local embedder")?;
                let local = local.ok_or("a local embedder needs the ONNX runtime")?;
                Ok(Self::Local(Box::new(Encoder::load(
                    local.environment,
                    local.models,
                    &pin,
                    local.threads,
                )?)))
            }
            EmbedderConfig::OpenAi(remote) => {
                let endpoint = Endpoint::resolve(data_dir, remote)?;
                Self::remote((**remote).clone(), endpoint)
            }
        }
    }

    pub fn remote(remote: Remote, endpoint: Endpoint) -> Result<Self, Error> {
        let mut model = Box::new(RemoteModel::new(remote, endpoint));
        let dims = match model.config.dimensions {
            Some(d) => d,
            None => model
                .embed(&["dimension probe".to_owned()], &Map::new())?
                .remove(0)
                .len(),
        };
        Ok(Self::Remote { model, dims })
    }

    pub fn dims(&self) -> usize {
        match self {
            Self::Local(encoder) => encoder.pin().dims,
            Self::Remote { dims, .. } => *dims,
        }
    }

    fn budget(&self) -> usize {
        match self {
            Self::Local(encoder) => encoder
                .pin()
                .max_tokens
                .saturating_sub(RESERVED_TOKENS)
                .max(16),
            Self::Remote { model, .. } => model
                .config
                .piece_tokens
                .unwrap_or(REMOTE_PIECE_TOKENS)
                .saturating_sub(RESERVED_TOKENS)
                .max(16),
        }
    }

    pub fn stamp(&self) -> Stamp {
        let (provider, model, counter, prefix) = match self {
            Self::Local(encoder) => (
                "local".to_owned(),
                encoder.pin().name.to_owned(),
                format!("tokenizer:{}", encoder.pin().name),
                encoder.pin().passage_prefix.to_owned(),
            ),
            Self::Remote { model, .. } => (
                format!(
                    "openai:{}",
                    model.config.profile.split('/').next().unwrap_or_default()
                ),
                model.config.model.clone(),
                "approximate-v1".to_owned(),
                format!(
                    "{}{}",
                    model.config.passage_prefix,
                    serde_json::to_string(&model.config.passage_body).unwrap_or_default()
                ),
            ),
        };
        use sha2::{Digest, Sha256};
        let prefix = format!("{:x}", Sha256::digest(prefix.as_bytes()));
        Stamp {
            provider,
            model,
            dimensions: self.dims(),
            preprocessing: format!(
                "readable-v1;pieces={MAX_PIECES};budget={};counter={counter};prefix={}",
                self.budget(),
                &prefix[..12]
            ),
        }
    }

    pub fn chunks_of(&self, raw: &[u8]) -> Result<Vec<Chunk>, Error> {
        let spans = project(raw);
        let budget = self.budget();
        match self {
            Self::Local(encoder) => {
                let tokenizer = encoder.counter();
                let mut count = |text: &str| -> Result<Vec<(usize, usize)>, Error> {
                    Ok(tokenizer
                        .encode(text, false)
                        .map_err(|e| e.to_string())?
                        .get_offsets()
                        .to_vec())
                };
                chunk(&spans, &mut count, budget, MAX_PIECES)
            }
            Self::Remote { .. } => {
                let mut count = |text: &str| -> Result<Vec<(usize, usize)>, Error> {
                    Ok(approximate_tokens(text))
                };
                chunk(&spans, &mut count, budget, MAX_PIECES)
            }
        }
    }

    pub fn batch(&self) -> usize {
        match self {
            Self::Local(_) => 16,
            Self::Remote { .. } => REMOTE_BATCH,
        }
    }

    pub fn embed_queries(&mut self, queries: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        let budget = self.budget();
        match self {
            Self::Local(encoder) => encoder.embed_queries(queries),
            Self::Remote { model, .. } => {
                let texts: Vec<String> = queries
                    .iter()
                    .map(|q| format!("{}{}", model.config.query_prefix, cut_to_tokens(q, budget)))
                    .collect();
                let extra = model.config.query_body.clone();
                model.embed(&texts, &extra)
            }
        }
    }

    pub fn embed_passages(&mut self, passages: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        match self {
            Self::Local(encoder) => encoder.embed_passages(passages),
            Self::Remote { model, .. } => {
                let texts: Vec<String> = passages
                    .iter()
                    .map(|p| format!("{}{p}", model.config.passage_prefix))
                    .collect();
                let extra = model.config.passage_body.clone();
                model.embed(&texts, &extra)
            }
        }
    }

    pub fn usage(&self) -> Usage {
        match self {
            Self::Local(_) => Usage::default(),
            Self::Remote { model, .. } => model.usage,
        }
    }

    pub fn price(&self) -> Option<f64> {
        match self {
            Self::Local(_) => Some(0.0),
            Self::Remote { model, .. } => model.config.usd_per_million_tokens,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }
}

// ---------------------------------------------------------------- reranker

/// A loaded local or remote cross-encoder reranker.
pub enum Reranker {
    Local(Box<Encoder>),
    Remote(Box<RemoteModel>),
}

impl Reranker {
    pub fn load(
        config: &RerankerConfig,
        data_dir: &Path,
        local: Option<&Local<'_>>,
    ) -> Result<Self, Error> {
        match config {
            RerankerConfig::Local { .. } => {
                let pin = local_reranker_pin(config)?.ok_or("no local reranker")?;
                let local = local.ok_or("a local reranker needs the ONNX runtime")?;
                Ok(Self::Local(Box::new(Encoder::load(
                    local.environment,
                    local.models,
                    &pin,
                    local.threads,
                )?)))
            }
            RerankerConfig::Rerank(remote) => Ok(Self::Remote(Box::new(RemoteModel::new(
                (**remote).clone(),
                Endpoint::resolve(data_dir, remote)?,
            )))),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Self::Local(encoder) => encoder.pin().name.to_owned(),
            Self::Remote(model) => format!("{} ({})", model.config.model, model.config.profile),
        }
    }

    pub fn score(&mut self, query: &str, passages: &[&str]) -> Result<Vec<f32>, Error> {
        match self {
            Self::Local(encoder) => {
                let mut scores = Vec::with_capacity(passages.len());
                for batch in passages.chunks(8) {
                    scores.extend(encoder.score(query, batch)?);
                }
                Ok(scores)
            }
            Self::Remote(model) => model.rerank(query, passages),
        }
    }

    pub fn usage(&self) -> Usage {
        match self {
            Self::Local(_) => Usage::default(),
            Self::Remote(model) => model.usage,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[allow(clippy::type_complexity)]
    pub(crate) fn serve(
        answer: impl Fn(&str, &Value) -> (u16, Value) + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<(String, String, Value)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Ok(cloned) = stream.try_clone() else {
                    break;
                };
                let mut reader = BufReader::new(cloned);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }
                let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
                let mut length = 0;
                let mut auth = String::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() {
                        break;
                    }
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let lower = header.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization:") {
                        auth = header["authorization:".len()..].trim().to_owned();
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                let Ok(body) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let (status, reply) = answer(&path, &body);
                record.lock().unwrap().push((path, auth, body));
                let text = reply.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        (url, seen)
    }

    #[test]
    fn profile_key_resolution_and_debug_redaction() {
        let dir = crate::test_tempdir();
        let profiles = dir.path().join("profiles");
        let or = profiles.join("openrouter/default");
        std::fs::create_dir_all(&or).unwrap();
        std::fs::write(or.join("api-key"), "sk-or-test\n").unwrap();
        let gem = profiles.join("google-gemini/api");
        std::fs::create_dir_all(&gem).unwrap();
        std::fs::write(gem.join("gemini-api-key"), "g-key\n").unwrap();
        let remote = |profile: &str| Remote {
            profile: profile.into(),
            model: "m".into(),
            ..Remote::default()
        };
        let e = Endpoint::resolve(dir.path(), &remote("openrouter/default")).unwrap();
        assert_eq!(e.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(e.key, "sk-or-test");
        let e = Endpoint::resolve(dir.path(), &remote("google-gemini/api")).unwrap();
        assert_eq!(e.key, "g-key");
        assert!(e.base_url.ends_with("/v1beta/openai"));
        assert!(Endpoint::resolve(dir.path(), &remote("openrouter/none")).is_err());
        assert!(Endpoint::resolve(dir.path(), &remote("../x")).is_err());
        assert!(Endpoint::resolve(dir.path(), &remote("openrouter/..")).is_err());
        let other = profiles.join("acme/k");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("api-key"), "a").unwrap();
        assert!(Endpoint::resolve(dir.path(), &remote("acme/k")).is_err());
        let mut with_root = remote("acme/k");
        with_root.base_url = Some("https://example.test/v1/".into());
        assert_eq!(
            Endpoint::resolve(dir.path(), &with_root).unwrap().base_url,
            "https://example.test/v1"
        );
        let e = Endpoint::resolve(dir.path(), &remote("openrouter/default")).unwrap();
        assert!(!format!("{e:?}").contains("sk-or-test"));
    }

    #[test]
    fn configuration_defaults_to_local_and_parses_remote_providers() {
        assert_eq!(
            ProviderConfig::load(Path::new("/nonexistent/config.json")).unwrap(),
            ProviderConfig::default()
        );
        let missing = ProviderConfig::load_required(Path::new("/nonexistent/config.json"));
        assert!(
            missing
                .unwrap_err()
                .to_string()
                .contains("/nonexistent/config.json")
        );
        let config: ProviderConfig = serde_json::from_value(json!({
            "embedder": {
                "provider": "openai",
                "profile": "openrouter/default",
                "model": "qwen/qwen3-embedding-8b",
                "dimensions": 1024,
                "usd_per_million_tokens": 0.01
            },
            "reranker": {
                "provider": "rerank",
                "profile": "openrouter/default",
                "model": "cohere/rerank-3.5",
                "usd_per_search": 0.002
            },
            "max_rebuild_usd": 3.0,
            "max_daily_usd": 1.0
        }))
        .unwrap();
        config.require_prices().unwrap();
        let EmbedderConfig::OpenAi(remote) = &config.embedder else {
            panic!("{config:?}")
        };
        assert_eq!(remote.dimensions, Some(1024));
        assert!(matches!(config.reranker, RerankerConfig::Rerank(_)));
        assert!(
            serde_json::from_value::<ProviderConfig>(json!({"embedder": {"provider": "openai",
                "profile": "a/b", "model": "m", "typo": 1}}))
            .is_err()
        );
        assert_eq!(
            local_embedder_pin(&EmbedderConfig::default())
                .unwrap()
                .unwrap()
                .name,
            crate::pins::EMBEDDER.name
        );
        assert!(
            local_embedder_pin(&EmbedderConfig::Local {
                model: Some(crate::pins::RERANKER.name.into())
            })
            .is_err()
        );
    }

    #[test]
    fn approximate_tokens_count_cjk_by_character_and_words_by_four_letters() {
        let text = "點解 ship it, 部署v2";
        let tokens: Vec<&str> = approximate_tokens(text)
            .into_iter()
            .map(|(s, e)| &text[s..e])
            .collect();
        assert_eq!(
            tokens,
            vec!["點", "解", "ship", "it", ",", "部", "署", "v2"]
        );
        assert_eq!(approximate_tokens("abcdefghij").len(), 3);
    }

    #[test]
    fn remote_embedder_and_reranker_enforce_openrouter_policy_and_query_budget() {
        let (url, seen) = serve(|path, body| match path {
            "/v1/embeddings" => {
                let n = body["input"].as_array().unwrap().len();
                let data: Vec<Value> = (0..n)
                    .rev()
                    .map(|i| json!({"index": i, "embedding": [3.0, 4.0 + i as f64]}))
                    .collect();
                (
                    200,
                    json!({"data": data, "usage": {"prompt_tokens": 7 * n, "cost": 0.001}}),
                )
            }
            _ => {
                let n = body["documents"].as_array().unwrap().len();
                let results: Vec<Value> = (0..n)
                    .map(|i| json!({"index": i, "relevance_score": i as f64}))
                    .collect();
                (200, json!({"results": results}))
            }
        });
        let remote = Remote {
            profile: "openrouter/default".into(),
            model: "qwen/qwen3-embedding-8b".into(),
            piece_tokens: Some(116),
            query_prefix: "Q: ".into(),
            ..Remote::default()
        };
        let mut embedder =
            Embedder::remote(remote.clone(), Endpoint::new(format!("{url}/v1"), "k1")).unwrap();
        assert_eq!(embedder.dims(), 2);
        let long = "word ".repeat(1_000);
        let vectors = embedder.embed_queries(&[long.as_str()]).unwrap();
        assert_eq!(vectors.len(), 1);
        assert!((vectors[0][0] - 0.6).abs() < 1e-6);

        let mut reranker = Reranker::Remote(Box::new(RemoteModel::new(
            remote,
            Endpoint::new(format!("{url}/v1"), "k1"),
        )));
        let scores = reranker.score(&long, &["a", "b"]).unwrap();
        assert_eq!(scores, vec![0.0, 1.0]);

        let requests = seen.lock().unwrap();
        for (_, auth, body) in requests.iter() {
            assert_eq!(auth, "Bearer k1");
            assert_eq!(body["provider"]["data_collection"], json!("deny"));
        }
        let query_sent = requests[1].2["input"][0].as_str().unwrap();
        let trimmed = query_sent.strip_prefix("Q: ").unwrap();
        assert_eq!(approximate_tokens(trimmed).len(), 100);
    }

    #[test]
    fn remote_rerank_transmits_full_utf8_query_and_passages() {
        let (url, seen) = serve(|path, _| {
            assert_eq!(path, "/v1/rerank");
            (
                200,
                json!({"results": [
                    {"index": 2, "relevance_score": 0.75},
                    {"index": 0, "relevance_score": 0.25},
                    {"index": 1, "relevance_score": 0.5},
                ]}),
            )
        });
        let mut remote = RemoteModel::new(
            Remote {
                query_prefix: "Q: ".into(),
                passage_prefix: "P: ".into(),
                ..Remote::default()
            },
            Endpoint::new(format!("{url}/v1"), "example-token"),
        );
        let shared = "界🙂e\u{301}".repeat(1100);
        let query = format!("{shared}\nquery\0tail");
        let a = format!("{shared}\nfirst\0tail");
        let b = format!("{shared}\nsecond\0tail");
        assert_eq!(
            remote.rerank(&query, &[&a, &b, "short"]).unwrap(),
            [0.25, 0.5, 0.75]
        );
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let (_, _, body) = &requests[0];
        assert_eq!(
            body["query"].as_str().unwrap().as_bytes(),
            format!("Q: {query}").as_bytes()
        );
        let actual = body["documents"].as_array().unwrap();
        for (actual, expected) in
            actual
                .iter()
                .zip([format!("P: {a}"), format!("P: {b}"), "P: short".into()])
        {
            assert_eq!(actual.as_str().unwrap().as_bytes(), expected.as_bytes());
        }
        assert_eq!(actual.len(), 3);
        assert_ne!(
            actual[0], actual[1],
            "same-prefix passages must not collapse"
        );
    }

    #[test]
    fn remote_rerank_rejects_over_limit_or_replaced_evidence_without_sending() {
        let (url, seen) = serve(|_, _| (400, json!({"error": "unexpected request"})));
        let mut remote = RemoteModel::new(
            Remote {
                passage_chars: Some(1000),
                ..Remote::default()
            },
            Endpoint::new(format!("{url}/v1"), "example-token"),
        );
        let long = "界".repeat(1001);
        let short = "🙂".repeat(1000);
        let err = remote
            .rerank(&short, &[&short, &long])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("passage at index 1 has 1001 characters, exceeding configured limit 1000"),
            "{err}"
        );
        let err = remote.rerank(&long, &[&short]).unwrap_err().to_string();
        assert!(
            err.contains("query has 1001 characters, exceeding configured limit 1000"),
            "{err}"
        );
        remote
            .config
            .body
            .insert("documents".into(), json!(["replacement"]));
        let err = remote.rerank(&short, &[&short]).unwrap_err().to_string();
        assert!(err.contains("must not override documents"), "{err}");
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(remote.usage.requests, 0);
    }
}
