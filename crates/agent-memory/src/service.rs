//! Standalone memory retrieval service: embeds raw history in striped background
//! workers and serves dense search and cross-encoder rerank requests over an
//! owner-only Unix domain socket.

use crate::{
    dense::DenseIndex,
    embed::{HostCatalog, Stripe, embed_documents},
    host_index::INDEX_FILE,
    model::{self, ModelPin},
    protocol::{self, Rebuild, Reply, Request, Status},
    providers::{
        DEFAULT_MAX_DAILY_USD, DEFAULT_MAX_REBUILD_USD, Embedder, EmbedderConfig, Local,
        ProviderConfig, Reranker, Stamp, local_embedder_pin, local_reranker_pin,
    },
    raw::read_raw,
    runtime::{self, Error},
    text::readable,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
};

/// Default filename of the host index inside `data_dir`.
pub const HOST_INDEX: &str = INDEX_FILE;

/// Service runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    /// Directory storing pinned local models and ONNX Runtime.
    pub models: PathBuf,
    /// Number of background embedding workers (one stripe each).
    pub index_threads: usize,
    /// Threads dedicated to query embedding and reranking.
    pub query_threads: usize,
    /// Background worker nice value on Unix.
    pub nice: i32,
    /// Disable background indexing and answer queries only.
    pub no_index: bool,
    /// Provider configuration (`<data>/memory-service/config.json`).
    pub providers: ProviderConfig,
    /// Idle poll interval for background workers.
    pub poll: Duration,
    /// Initial retry backoff for failing background steps.
    pub retry: Duration,
}

/// Maximum retry interval for a failing background worker.
pub const RETRY_MAX: Duration = Duration::from_secs(600);

impl Config {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_owned(),
            models: data_dir.join("memory-service").join("models"),
            index_threads: 1,
            query_threads: 2,
            nice: 19,
            no_index: false,
            providers: ProviderConfig::default(),
            poll: Duration::from_secs(30),
            retry: Duration::from_secs(30),
        }
    }

    pub fn dense_path(&self, stamp: &Stamp) -> PathBuf {
        self.data_dir
            .join("memory-service")
            .join(format!("dense-{}.db", stamp.digest()))
    }

    pub fn local_pins(&self) -> Result<Vec<ModelPin>, Error> {
        Ok(local_embedder_pin(&self.providers.embedder)?
            .into_iter()
            .chain(local_reranker_pin(&self.providers.reranker)?)
            .collect())
    }

    pub fn max_rebuild_usd(&self) -> f64 {
        self.providers
            .max_rebuild_usd
            .unwrap_or(DEFAULT_MAX_REBUILD_USD)
    }

    pub fn max_daily_usd(&self) -> f64 {
        self.providers
            .max_daily_usd
            .unwrap_or(DEFAULT_MAX_DAILY_USD)
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.data_dir.join("memory-service").join("spend.db")
    }
}

#[cfg(target_os = "linux")]
fn nice_this_thread(nice: i32) {
    // SAFETY: plain syscalls on the calling thread.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, nice);
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn nice_this_thread(nice: i32) {
    // SAFETY: lowers priority for the current process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, nice);
    }
}

#[cfg(not(unix))]
fn nice_this_thread(_nice: i32) {}

/// Initialize the ONNX Runtime environment when any local model is configured.
pub fn local_runtime(
    config: &Config,
) -> Result<Option<&'static ort::environment::Environment>, Error> {
    let pins = config.local_pins()?;
    if pins.is_empty() {
        return Ok(None);
    }
    model::check(&config.models, &pins)?;
    let runtime = model::runtime_pin().ok_or("no pinned ONNX Runtime for this platform")?;
    Ok(Some(runtime::init(&config.models, &runtime)?))
}

const SPENT: &str = "spent_microusd";
const BUILT: &str = "built_ms";
const ESTIMATE_SAMPLE: usize = 256;

/// Estimate rebuild token volume and cost by uniformly sampling the host catalog.
pub fn estimate(host: &HostCatalog, embedder: &Embedder, max_usd: f64) -> Result<Rebuild, Error> {
    let documents = host.count()?;
    let sample = host.sample(ESTIMATE_SAMPLE)?;
    let mut tokens = 0u64;
    let mut read = 0u64;
    for document in &sample {
        let Ok(raw) = read_raw(&document.raw) else {
            continue;
        };
        read += 1;
        tokens += embedder
            .chunks_of(&raw)?
            .iter()
            .map(|c| c.tokens as u64)
            .sum::<u64>();
    }
    let estimated_tokens = if read == 0 {
        0
    } else {
        (tokens as f64 / read as f64 * documents as f64).round() as u64
    };
    Ok(Rebuild {
        documents,
        sampled_documents: read,
        estimated_tokens,
        estimated_usd: embedder
            .price()
            .map(|per_million| per_million * estimated_tokens as f64 / 1e6),
        max_usd,
        state: String::new(),
        spent_usd: 0.0,
    })
}

/// Evaluate whether a rebuild is permitted to run under its cost cap.
pub fn rebuild_state(estimate: &Rebuild, remote: bool) -> String {
    match estimate.estimated_usd {
        _ if !remote => "running".into(),
        Some(usd) if usd <= estimate.max_usd => "running".into(),
        Some(usd) => format!(
            "waiting: estimated US${usd:.2} exceeds max_rebuild_usd US${:.2}",
            estimate.max_usd
        ),
        None => "waiting: the model's price is unknown; set usd_per_million_tokens".into(),
    }
}

/// Generation-scoped and worker-count-scoped progress marks for one stripe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marks {
    pub tail: String,
    pub backfill: String,
}

impl Marks {
    pub fn of(generation: &str, stripe: Stripe) -> Self {
        Self {
            tail: format!("tail/{generation}/{}/{}", stripe.of, stripe.index),
            backfill: format!("backfill/{generation}/{}/{}", stripe.of, stripe.index),
        }
    }
}

fn backlog_done(dense: &DenseIndex, generation: &str, workers: usize) -> Result<bool, Error> {
    let of = workers.max(1);
    for index in 0..of {
        let marks = Marks::of(generation, Stripe { index, of });
        if dense.progress(&marks.backfill)? != Some(0) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn built(dense: &DenseIndex, generation: &str, workers: usize) -> Result<bool, Error> {
    if dense.progress(BUILT)?.is_some() {
        return Ok(true);
    }
    if !backlog_done(dense, generation, workers)? {
        return Ok(false);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_millis() as i64);
    dense.set_progress(BUILT, now)?;
    Ok(true)
}

/// Compute a rebuild plan if the vector space has not yet completed its initial backlog.
pub fn plan_rebuild(
    host_path: &Path,
    dense: &DenseIndex,
    embedder: &Embedder,
    max_usd: f64,
    workers: usize,
) -> Result<Option<Rebuild>, Error> {
    if !host_path.exists() {
        return Ok(None);
    }
    let host = HostCatalog::open(host_path)?;
    if built(dense, &host.generation()?, workers)? {
        return Ok(None);
    }
    let mut rebuild = estimate(&host, embedder, max_usd)?;
    rebuild.spent_usd = dense.progress(SPENT)?.unwrap_or(0) as f64 / 1e6;
    rebuild.state = rebuild_state(&rebuild, embedder.is_remote());
    Ok(Some(rebuild))
}

/// Remote spend totals compared against configured caps.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Budget {
    pub rebuild_usd: f64,
    pub max_rebuild_usd: f64,
    pub today_usd: f64,
    pub max_daily_usd: f64,
}

/// Enforce rebuild and daily spend caps before invoking remote models.
pub fn gate(
    rebuild: Option<&Rebuild>,
    remote: bool,
    building: bool,
    budget: Budget,
) -> Result<(), String> {
    if !remote {
        return Ok(());
    }
    if let Some(rebuild) = rebuild.filter(|r| r.state != "running") {
        return Err(rebuild.state.clone());
    }
    if building && budget.rebuild_usd >= budget.max_rebuild_usd {
        return Err(format!(
            "stopped: reported spend US${:.2} reached max_rebuild_usd US${:.2}",
            budget.rebuild_usd, budget.max_rebuild_usd
        ));
    }
    if !building && budget.today_usd >= budget.max_daily_usd {
        return Err(daily_cap_reached(budget.today_usd, budget.max_daily_usd));
    }
    Ok(())
}

fn daily_cap_reached(today_usd: f64, max_daily_usd: f64) -> String {
    format!(
        "paused: US${today_usd:.2} spent today reached max_daily_usd US${max_daily_usd:.2}; resumes at 00:00 UTC"
    )
}

fn reported_state(rebuild: &Rebuild, built: bool, remote: bool, error: Option<&str>) -> String {
    if rebuild.state != "running" {
        return rebuild.state.clone();
    }
    if built {
        return "done".into();
    }
    if remote && rebuild.spent_usd >= rebuild.max_usd {
        return format!(
            "stopped: reported spend US${:.2} reached max_rebuild_usd US${:.2}",
            rebuild.spent_usd, rebuild.max_usd
        );
    }
    if let Some(error) = error {
        return format!("retrying: {error}");
    }
    "running".into()
}

/// Persistent UTC-day spend ledger stored in SQLite WAL.
pub struct Ledger {
    conn: Mutex<Connection>,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS daily(day INTEGER PRIMARY KEY, usd REAL NOT NULL);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn day_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| (d.as_secs() / 86_400) as i64)
    }

    pub fn spent_on(&self, day: i64) -> Result<f64, Error> {
        let conn = self.conn.lock().map_err(|_| "ledger poisoned")?;
        Ok(conn
            .query_row("SELECT usd FROM daily WHERE day=?1", [day], |r| r.get(0))
            .optional()?
            .unwrap_or(0.0))
    }

    pub fn charge_on(&self, day: i64, usd: f64) -> Result<f64, Error> {
        if usd <= 0.0 {
            return self.spent_on(day);
        }
        let conn = self.conn.lock().map_err(|_| "ledger poisoned")?;
        Ok(conn.query_row(
            "INSERT INTO daily VALUES(?1,?2) ON CONFLICT(day) DO UPDATE SET usd=usd+excluded.usd
             RETURNING usd",
            params![day, usd],
            |r| r.get(0),
        )?)
    }

    pub fn today(&self) -> Result<f64, Error> {
        self.spent_on(Self::day_now())
    }

    pub fn charge(&self, usd: f64) -> Result<f64, Error> {
        self.charge_on(Self::day_now(), usd)
    }
}

/// Offline preview estimate for the configured embedder.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Estimate {
    pub embedder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dense_index: Option<PathBuf>,
    pub built: bool,
    pub rebuild: Rebuild,
}

fn configured_embedder(config: &Config) -> Result<(Embedder, String, bool), Error> {
    match &config.providers.embedder {
        EmbedderConfig::OpenAi(remote) => {
            let mut offline = (**remote).clone();
            let known = offline.dimensions.is_some();
            offline.dimensions = Some(offline.dimensions.unwrap_or(1));
            Ok((
                Embedder::remote(offline, crate::providers::Endpoint::offline())?,
                format!("{} ({})", remote.model, remote.profile),
                known,
            ))
        }
        local => {
            let environment = local_runtime(config)?;
            let runtime = environment.map(|environment| Local {
                environment,
                models: &config.models,
                threads: 1,
            });
            let embedder = Embedder::load(local, &config.data_dir, runtime.as_ref())?;
            let name = embedder.stamp().model;
            Ok((embedder, name, true))
        }
    }
}

fn recorded_built(path: &Path, stamp: &Stamp) -> Result<bool, Error> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(
        match DenseIndex::open_read_only(path, &stamp.key(), stamp.dimensions)? {
            Some(dense) => dense.progress(BUILT)?.is_some(),
            None => false,
        },
    )
}

/// Estimate what building the configured embedder's space would read and cost
/// without calling any remote model or mutating disk state.
pub fn estimate_configured(config: &Config) -> Result<Estimate, Error> {
    let host_path = config.data_dir.join(HOST_INDEX);
    if !host_path.exists() {
        return Err(format!("no host index at {}", host_path.display()).into());
    }
    let (embedder, name, width_known) = configured_embedder(config)?;
    let stamp = embedder.stamp();
    let dense_path = config.dense_path(&stamp);
    let built = width_known && recorded_built(&dense_path, &stamp)?;
    let host = HostCatalog::open(&host_path)?;
    let mut rebuild = estimate(&host, &embedder, config.max_rebuild_usd())?;
    rebuild.state = if built {
        "done".into()
    } else {
        rebuild_state(&rebuild, embedder.is_remote())
    };
    Ok(Estimate {
        embedder: name,
        space: width_known.then(|| stamp.key()),
        dense_index: width_known.then_some(dense_path),
        built,
        rebuild,
    })
}

/// Remove dense index files of superseded vector spaces once the active space is built.
pub fn prune(config: &Config) -> Result<Vec<PathBuf>, Error> {
    let (embedder, name, width_known) = configured_embedder(config)?;
    if !width_known {
        return Err(format!(
            "the space of {name} is learned from the model; set dimensions to prune"
        )
        .into());
    }
    let stamp = embedder.stamp();
    let keep = config.dense_path(&stamp);
    if !recorded_built(&keep, &stamp)? {
        return Err(format!(
            "{name}'s space ({}) is not built yet; nothing removed",
            keep.display()
        )
        .into());
    }
    prune_other_spaces(&keep)
}

#[cfg(unix)]
struct Shared {
    config: Config,
    stamp: Stamp,
    embedder_name: String,
    embedder: Mutex<Embedder>,
    reranker: Mutex<Reranker>,
    rebuild: Mutex<Option<Rebuild>>,
    spent_microusd: std::sync::atomic::AtomicU64,
    ledger: Ledger,
    workers: Mutex<Vec<WorkerState>>,
    stop: AtomicBool,
}

#[cfg(unix)]
#[derive(Clone, Debug, Default)]
struct WorkerState {
    error: Option<String>,
    waiting: Option<String>,
}

#[cfg(unix)]
impl Shared {
    fn dense(&self) -> Result<DenseIndex, Error> {
        Ok(DenseIndex::open(
            &self.config.dense_path(&self.stamp),
            &self.stamp.key(),
            self.stamp.dimensions,
        )?)
    }

    fn set_worker(&self, index: usize, update: impl FnOnce(&mut WorkerState)) {
        if let Ok(mut workers) = self.workers.lock()
            && let Some(state) = workers.get_mut(index)
        {
            update(state);
        }
    }

    fn status(&self) -> Result<Status, Error> {
        let dense = self.dense()?;
        let (embedded, vectors) = dense.counts()?;
        let host = self.config.data_dir.join(HOST_INDEX);
        let (total, generation) = if host.exists() {
            let host = HostCatalog::open(&host)?;
            (host.count()?, host.generation()?)
        } else {
            (0, "legacy".to_owned())
        };
        let (query_usd, remote) = self
            .embedder
            .lock()
            .map_or((0.0, false), |e| (e.usage().usd, e.is_remote()));
        let query_usd = query_usd + self.reranker.lock().map_or(0.0, |r| r.usage().usd);
        let workers = self.workers.lock().map_err(|_| "poisoned")?.clone();
        let errors: Vec<String> = workers
            .iter()
            .enumerate()
            .filter_map(|(i, w)| w.error.as_ref().map(|e| format!("worker {i}: {e}")))
            .collect();
        let indexing = if self.config.no_index {
            "off".to_owned()
        } else if !errors.is_empty() {
            format!("retrying: {}", errors[0])
        } else if let Some(waiting) = workers.iter().find_map(|w| w.waiting.clone()) {
            waiting
        } else if total > embedded {
            "running".to_owned()
        } else {
            "idle".to_owned()
        };
        let built = built(&dense, &generation, self.config.index_threads.max(1))?;
        let mut rebuild = self.rebuild.lock().map_err(|_| "poisoned")?.clone();
        if let Some(rebuild) = &mut rebuild {
            rebuild.spent_usd = dense.progress(SPENT)?.unwrap_or(0) as f64 / 1e6;
            rebuild.state =
                reported_state(rebuild, built, remote, errors.first().map(String::as_str));
        }
        Ok(Status {
            embedder: self.embedder_name.clone(),
            reranker: self
                .reranker
                .lock()
                .map_or_else(|_| String::new(), |r| r.name()),
            embedded_documents: embedded,
            pending_documents: total.saturating_sub(embedded),
            vectors,
            dense_space: self.stamp.key(),
            rebuild,
            spent_usd: query_usd + self.spent_microusd.load(Ordering::Relaxed) as f64 / 1e6,
            indexing,
            errors,
            today_usd: self.ledger.today()?,
            max_daily_usd: self.config.max_daily_usd(),
        })
    }

    fn handle(&self, request: Request) -> Result<Reply, Error> {
        match request {
            Request::Status => Ok(Reply::Status {
                status: self.status()?,
            }),
            Request::Search {
                query,
                owners,
                source,
                since_ms,
                until_ms,
                limit,
            } => {
                let vector = {
                    let mut embedder = self.embedder.lock().map_err(|_| "embedder poisoned")?;
                    if embedder.is_remote() {
                        let today = self.ledger.today()?;
                        let cap = self.config.max_daily_usd();
                        if today >= cap {
                            return Err(daily_cap_reached(today, cap).into());
                        }
                    }
                    let before = embedder.usage().usd;
                    let embedded = embedder.embed_queries(&[query.as_str()]);
                    self.ledger.charge(embedder.usage().usd - before)?;
                    embedded?.remove(0)
                };
                let hits = self.dense()?.search(
                    &vector,
                    &owners,
                    source.as_deref(),
                    since_ms,
                    until_ms,
                    limit.min(200),
                )?;
                Ok(Reply::Search {
                    hits,
                    status: self.status()?,
                })
            }
            Request::DenseScores { query, passages } => {
                protocol::check_dense_inputs(&query, &passages)?;
                // Do not queue behind other expensive requests: callers can fall back
                // to their local lexical path rather than stacking timed-out work.
                let mut embedder = self
                    .embedder
                    .try_lock()
                    .map_err(|_| "embedder unavailable")?;
                if embedder.is_remote() {
                    return Err("scoped dense scoring requires a local embedder".into());
                }
                let texts: Vec<String> = passages.iter().map(|p| readable(p.as_bytes())).collect();
                let query_vectors = embedder.embed_queries(&[&query])?;
                let [query_vector] = query_vectors.as_slice() else {
                    return Err("invalid scoped query embedding".into());
                };
                let mut similarities = Vec::with_capacity(texts.len());
                for batch in texts.chunks(embedder.batch()) {
                    let refs: Vec<&str> = batch.iter().map(String::as_str).collect();
                    let vectors = embedder.embed_passages(&refs)?;
                    if vectors.len() != refs.len() {
                        return Err("invalid scoped passage embeddings".into());
                    }
                    for vector in vectors {
                        similarities.push(
                            crate::dense::transient_similarity(query_vector, &vector)
                                .ok_or("invalid scoped vector similarity")?,
                        );
                    }
                }
                Ok(Reply::DenseScores { similarities })
            }
            Request::Rerank { query, passages } => {
                let texts: Vec<String> = passages.iter().map(|p| readable(p.as_bytes())).collect();
                let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
                let mut reranker = self.reranker.lock().map_err(|_| "reranker poisoned")?;
                if matches!(*reranker, Reranker::Remote(_)) {
                    let today = self.ledger.today()?;
                    let cap = self.config.max_daily_usd();
                    if today >= cap {
                        return Err(daily_cap_reached(today, cap).into());
                    }
                }
                let before = reranker.usage().usd;
                let scores = reranker.score(&query, &refs);
                self.ledger.charge(reranker.usage().usd - before)?;
                Ok(Reply::Rerank { scores: scores? })
            }
        }
    }
}

#[cfg(unix)]
fn serve_connection(shared: &Shared, stream: UnixStream) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let reply = protocol::decode_request(line.trim())
        .and_then(|request| shared.handle(request).map_err(|e| e.to_string()));
    let _ = (&stream).write_all(protocol::encode_reply(&reply).as_bytes());
}

#[cfg(unix)]
fn bind(socket: &Path) -> std::io::Result<UnixListener> {
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("a memory service already answers on {}", socket.display()),
            ));
        }
        std::fs::remove_file(socket)?;
    }
    // Serialize process-global umask changes across threads and mask only group/other
    // bits (0o077, not 0o177) so concurrent `mkdir`/`create_dir_all` calls in other
    // threads never lose the owner traverse (+x) bit. The socket is still strictly
    // owner-only at creation (0o700) and immediately tightened to 0o600 below.
    static UMASK_LOCK: Mutex<()> = Mutex::new(());
    let bound = {
        let _guard = UMASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: plain umask syscall restored immediately around bind().
        let previous = unsafe { libc::umask(0o077) };
        let res = UnixListener::bind(socket);
        unsafe { libc::umask(previous) };
        res
    };
    let listener = bound?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Bind the owner-only Unix socket and serve requests until stopped.
#[cfg(unix)]
pub fn serve(config: Config) -> Result<(), Error> {
    let listener = bind(&protocol::socket_path(&config.data_dir))?;
    run(listener, config)
}

/// Serve requests on an already-bound Unix listener with optional background indexing.
#[cfg(unix)]
pub fn run(listener: UnixListener, config: Config) -> Result<(), Error> {
    config.providers.require_prices()?;
    let environment = local_runtime(&config)?;
    let local = environment.map(|environment| Local {
        environment,
        models: &config.models,
        threads: config.query_threads,
    });
    let embedder = Embedder::load(&config.providers.embedder, &config.data_dir, local.as_ref())?;
    let reranker = Reranker::load(&config.providers.reranker, &config.data_dir, local.as_ref())?;
    let stamp = embedder.stamp();
    let embedder_name = match &config.providers.embedder {
        EmbedderConfig::Local { .. } => stamp.model.clone(),
        EmbedderConfig::OpenAi(remote) => format!("{} ({})", remote.model, remote.profile),
    };
    let shared = Arc::new(Shared {
        embedder_name,
        stamp: stamp.clone(),
        embedder: Mutex::new(embedder),
        reranker: Mutex::new(reranker),
        rebuild: Mutex::new(None),
        spent_microusd: std::sync::atomic::AtomicU64::new(0),
        ledger: Ledger::open(&config.ledger_path())?,
        workers: Mutex::new(vec![WorkerState::default(); config.index_threads.max(1)]),
        config: config.clone(),
        stop: AtomicBool::new(false),
    });
    if !config.no_index {
        let host_path = config.data_dir.join(HOST_INDEX);
        let planned = {
            let embedder = shared.embedder.lock().map_err(|_| "embedder poisoned")?;
            plan_rebuild(
                &host_path,
                &shared.dense()?,
                &embedder,
                config.max_rebuild_usd(),
                config.index_threads.max(1),
            )?
        };
        if let Some(rebuild) = planned {
            let path = config.dense_path(&stamp).with_extension("estimate.json");
            std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
            std::fs::write(&path, serde_json::to_vec_pretty(&rebuild)?)?;
            *shared.rebuild.lock().map_err(|_| "poisoned")? = Some(rebuild);
        }
        let workers = config.index_threads.max(1);
        for index in 0..workers {
            let indexer = Arc::clone(&shared);
            let stripe = Stripe { index, of: workers };
            std::thread::Builder::new()
                .name(format!("memory-indexer-{index}"))
                .spawn(move || {
                    nice_this_thread(indexer.config.nice);
                    index_loop(&indexer, environment, stripe);
                })?;
        }
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || serve_connection(&shared, stream));
    }
    shared.stop.store(true, Ordering::Relaxed);
    Ok(())
}

const SWEEP_EVERY: Duration = Duration::from_secs(600);
const BATCH: usize = 64;

#[cfg(unix)]
fn index_loop(
    shared: &Shared,
    environment: Option<&'static ort::environment::Environment>,
    stripe: Stripe,
) {
    let mut retry = shared.config.retry;
    let mut worker = None;
    let mut last_sweep = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) {
        match index_once(shared, environment, stripe, &mut worker, &mut last_sweep) {
            Ok(wait) => {
                shared.set_worker(stripe.index, |w| w.error = None);
                retry = shared.config.retry;
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            }
            Err(error) => {
                let message = format!("{error} (next try in {} s)", retry.as_secs_f64().ceil());
                shared.set_worker(stripe.index, |w| w.error = Some(message));
                std::thread::sleep(retry);
                retry = (retry * 2).min(RETRY_MAX);
            }
        }
    }
}

#[cfg(unix)]
fn index_once(
    shared: &Shared,
    environment: Option<&'static ort::environment::Environment>,
    stripe: Stripe,
    worker: &mut Option<(Embedder, DenseIndex)>,
    last_sweep: &mut Instant,
) -> Result<Duration, Error> {
    let config = &shared.config;
    let host_path = config.data_dir.join(HOST_INDEX);
    if !host_path.exists() {
        return Ok(config.poll);
    }
    if worker.is_none() {
        let local = environment.map(|environment| Local {
            environment,
            models: &config.models,
            threads: 1,
        });
        let encoder = Embedder::load(&config.providers.embedder, &config.data_dir, local.as_ref())?;
        if encoder.stamp() != shared.stamp {
            return Err("the embedder changed its space while loading".into());
        }
        *worker = Some((encoder, shared.dense()?));
    }
    let Some((encoder, dense)) = worker.as_mut() else {
        return Ok(config.poll);
    };
    let host = HostCatalog::open(&host_path)?;
    let building = !built(dense, &host.generation()?, stripe.of)?;
    let budget = Budget {
        rebuild_usd: dense.progress(SPENT)?.unwrap_or(0) as f64 / 1e6,
        max_rebuild_usd: config.max_rebuild_usd(),
        today_usd: shared.ledger.today()?,
        max_daily_usd: config.max_daily_usd(),
    };
    let allowed = {
        let rebuild = shared.rebuild.lock().map_err(|_| "poisoned")?;
        gate(rebuild.as_ref(), encoder.is_remote(), building, budget)
    };
    if let Err(reason) = allowed {
        shared.set_worker(stripe.index, |w| w.waiting = Some(reason));
        return Ok(config.poll);
    }
    shared.set_worker(stripe.index, |w| w.waiting = None);
    let before = encoder.usage().usd;
    let step = index_step(&host, encoder, dense, stripe, true);
    let spent = encoder.usage().usd - before;
    if spent > 0.0 {
        let micro = (spent * 1e6).round() as i64;
        shared
            .spent_microusd
            .fetch_add(micro as u64, Ordering::Relaxed);
        if building {
            dense.add_progress(SPENT, micro)?;
        } else {
            shared.ledger.charge(spent)?;
        }
    }
    let did = step?;
    if stripe.index == 0 && last_sweep.elapsed() >= SWEEP_EVERY {
        sweep(&host, dense)?;
        *last_sweep = Instant::now();
    }
    Ok(if did == 0 {
        config.poll
    } else {
        Duration::ZERO
    })
}

/// One bounded background indexing step for `stripe`; embeds fresh documents
/// first and historical backlog second.
pub fn index_step(
    host: &HostCatalog,
    encoder: &mut Embedder,
    dense: &mut DenseIndex,
    stripe: Stripe,
    backlog: bool,
) -> Result<usize, Error> {
    let marks = Marks::of(&host.generation()?, stripe);
    let tail = match dense.progress(&marks.tail)? {
        Some(tail) => tail,
        None => {
            let top = host.max_rowid()?;
            dense.set_progress(&marks.tail, top)?;
            dense.set_progress(&marks.backfill, top + 1)?;
            top
        }
    };
    let fresh = host.after(tail, BATCH, stripe)?;
    if !fresh.is_empty() {
        embed_documents(encoder, dense, &fresh, &marks.tail)?;
        return Ok(fresh.len());
    }
    if !backlog {
        return Ok(0);
    }
    let backfill = dense.progress(&marks.backfill)?.unwrap_or(0);
    let older = host.before(backfill, BATCH, stripe)?;
    embed_documents(encoder, dense, &older, &marks.backfill)?;
    if older.is_empty() && backfill > 0 {
        dense.set_progress(&marks.backfill, 0)?;
    }
    Ok(older.len())
}

/// Remove dense index files of other vector spaces beside `keep`.
pub fn prune_other_spaces(keep: &Path) -> Result<Vec<PathBuf>, Error> {
    let Some(dir) = keep.parent() else {
        return Ok(Vec::new());
    };
    let keep_name = keep
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let mut removed = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let base = name
            .strip_suffix("-wal")
            .or_else(|| name.strip_suffix("-shm"))
            .unwrap_or(name);
        let estimate = base
            .strip_suffix(".estimate.json")
            .map(|b| format!("{b}.db"));
        let base = estimate.as_deref().unwrap_or(base);
        if base.starts_with("dense-") && base.ends_with(".db") && base != keep_name {
            std::fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    removed.sort();
    Ok(removed)
}

/// Drop vectors for documents no longer present at the same digest in `host`.
pub fn sweep(host: &HostCatalog, dense: &DenseIndex) -> Result<usize, Error> {
    let mut stale = Vec::new();
    for (id, sha) in dense.documents()? {
        if !host.contains(&id, &sha)? {
            stale.push(id);
        }
    }
    Ok(dense.forget(&stale)?)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        host_index::HostIndex,
        providers::{Remote, RerankerConfig},
        raw::retained_jsonl_records,
        tokenize::{HistorySearchQuery, IndexMetadata, MemoryScope},
    };
    use serde_json::{Value, json};

    fn host_with(dir: &Path, lines: usize) -> PathBuf {
        let file = dir.join("history.jsonl");
        let text: String = (0..lines)
            .map(|i| format!("{{\"content\":\"message number {i} about deploys\"}}\n"))
            .collect();
        std::fs::write(&file, text).unwrap();
        let path = dir.join(HOST_INDEX);
        let mut index = HostIndex::open(&path).unwrap();
        for (i, raw) in retained_jsonl_records(&file).unwrap().iter().enumerate() {
            let metadata = IndexMetadata {
                agent_id: "agt".into(),
                source: MemoryScope::Transcripts,
                timestamp_ms: Some(i as i64),
                direction: "in".into(),
                sender: "s".into(),
                destination: "d".into(),
                tool_name: None,
                tool_names: Vec::new(),
                role: None,
            };
            index.index(&format!("doc{i}"), &metadata, raw).unwrap();
        }
        path
    }

    fn host_docs(dir: &Path, docs: &[(&str, &str)]) -> PathBuf {
        let file = dir.join("docs.jsonl");
        let text: String = docs
            .iter()
            .map(|(_, text)| json!({"content": text}).to_string() + "\n")
            .collect();
        std::fs::write(&file, text).unwrap();
        let path = dir.join(HOST_INDEX);
        let mut index = HostIndex::open(&path).unwrap();
        for (i, raw) in retained_jsonl_records(&file).unwrap().iter().enumerate() {
            let metadata = IndexMetadata {
                agent_id: docs[i].0.into(),
                source: MemoryScope::Transcripts,
                timestamp_ms: Some(i as i64),
                direction: "in".into(),
                sender: "s".into(),
                destination: "d".into(),
                tool_name: None,
                tool_names: Vec::new(),
                role: None,
            };
            index
                .index(&format!("{}-{i}", docs[i].0), &metadata, raw)
                .unwrap();
        }
        path
    }

    fn openrouter_profile(dir: &Path) {
        let key = dir.join("profiles/openrouter/default/api-key");
        std::fs::create_dir_all(key.parent().unwrap()).unwrap();
        std::fs::write(key, "sk-or-test\n").unwrap();
    }

    fn remote_pair(url: &str, usd_per_million_tokens: Option<f64>, max: f64) -> ProviderConfig {
        ProviderConfig {
            embedder: EmbedderConfig::OpenAi(Box::new(Remote {
                profile: "openrouter/default".into(),
                model: "qwen/qwen3-embedding-8b".into(),
                base_url: Some(format!("{url}/v1")),
                dimensions: Some(2),
                usd_per_million_tokens,
                query_prefix: "Q: ".into(),
                ..Remote::default()
            })),
            reranker: RerankerConfig::Rerank(Box::new(Remote {
                profile: "openrouter/default".into(),
                model: "cohere/rerank-3.5".into(),
                base_url: Some(format!("{url}/v1")),
                usd_per_million_tokens: Some(0.05),
                ..Remote::default()
            })),
            max_rebuild_usd: Some(max),
            max_daily_usd: None,
        }
    }

    fn start(config: Config) -> protocol::Client {
        let socket = protocol::socket_path(&config.data_dir);
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || run(listener, config));
        protocol::Client::new(socket, Duration::from_secs(30))
    }

    fn status_of(client: &protocol::Client) -> Status {
        match client.call(&Request::Status).unwrap() {
            Reply::Status { status } => status,
            other => panic!("{other:?}"),
        }
    }

    fn wait_for(client: &protocol::Client, done: impl Fn(&Status) -> bool) -> Status {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = status_of(client);
            if done(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "timed out: {status:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn hybrid_retrieval_fuses_dense_with_bm25_and_reranks() {
        let dir = crate::test_tempdir();
        let texts = [
            ("agt", "deploy failed on staging"),
            ("agt", "部署失敗 要 rollback"),
            ("agt", "lunch plans"),
            ("agt", "deploy notes for friday"),
            ("peer", "部署 peer secret"),
        ];
        let host_path = host_docs(dir.path(), &texts);
        openrouter_profile(dir.path());
        let (url, seen) = crate::providers::tests::serve(|path, body: &Value| {
            if path == "/v1/rerank" {
                let scores: Vec<Value> = body["documents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(i, d)| {
                        let d = d.as_str().unwrap();
                        let score = if d.contains("friday") {
                            3.0
                        } else if d.contains("部署") {
                            2.0
                        } else if d.contains("staging") {
                            1.0
                        } else {
                            0.0
                        };
                        json!({"index": i, "relevance_score": score})
                    })
                    .collect();
                return (200, json!({"results": scores}));
            }
            let data: Vec<Value> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, text)| {
                    let text = text.as_str().unwrap();
                    let v = if text.starts_with("Q: ") || text.contains("部署") {
                        [1.0, 0.0]
                    } else if text.contains("deploy") {
                        [0.6, 0.8]
                    } else {
                        [0.0, 1.0]
                    };
                    json!({"index": i, "embedding": v})
                })
                .collect();
            (200, json!({"data": data, "usage": {"prompt_tokens": 4}}))
        });
        let mut config = Config::new(dir.path());
        config.providers = remote_pair(&url, Some(0.01), 10.0);
        let client = start(config);
        wait_for(&client, |s| {
            s.embedded_documents == 5 && s.pending_documents == 0
        });

        let owners = vec!["agt".to_owned()];
        let lexical: Vec<String> = HostIndex::open(&host_path)
            .unwrap()
            .search(
                &owners,
                MemoryScope::Transcripts,
                &HistorySearchQuery {
                    query: Some("deploy".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .into_iter()
            .map(|hit| hit.id)
            .collect();
        let (hits, _) = client
            .search("deploy", &owners, None, None, None, 10)
            .unwrap();
        let dense: Vec<String> = hits.iter().map(|h| h.document_id.clone()).collect();
        assert_eq!(dense[0], "agt-1");
        assert!(hits.iter().all(|h| h.agent_id == "agt"));

        let fused: Vec<String> =
            crate::fusion::reciprocal_rank(&[lexical, dense], crate::fusion::K)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
        let passages: Vec<String> = fused
            .iter()
            .map(|id| {
                let i: usize = id.rsplit('-').next().unwrap().parse().unwrap();
                texts[i].1.to_owned()
            })
            .collect();
        let scores = client.rerank("deploy", &passages).unwrap();
        let mut order: Vec<(f32, &String)> = scores.into_iter().zip(&fused).collect();
        order.sort_by(|a, b| b.0.total_cmp(&a.0));
        let reranked: Vec<&str> = order.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(reranked, ["agt-3", "agt-1", "agt-0", "agt-2"]);
        assert!(!seen.lock().unwrap().is_empty());
    }

    #[test]
    fn scoped_dense_scores_refuse_remote_provider_without_network_or_index_access() {
        let dir = crate::test_tempdir();
        openrouter_profile(dir.path());
        let (url, seen) = crate::providers::tests::serve(|_, _| (500, json!({})));
        let mut config = Config::new(dir.path());
        config.no_index = true;
        config.providers = remote_pair(&url, Some(0.01), 10.0);
        let client = start(config);
        let error = client
            .dense_scores("automobile", &["vehicle maintenance".into()])
            .unwrap_err();
        assert!(error.contains("requires a local embedder"), "{error}");
        assert!(seen.lock().unwrap().is_empty());
        assert!(!dir.path().join(HOST_INDEX).exists());
    }

    #[test]
    fn rebuild_cap_refuses_expensive_space_before_model_call() {
        let dir = crate::test_tempdir();
        host_with(dir.path(), 12);
        openrouter_profile(dir.path());
        let (url, seen) = crate::providers::tests::serve(|_, _| (500, json!({})));
        let mut config = Config::new(dir.path());
        config.providers = remote_pair(&url, Some(1e6), 1.0);

        let preview = estimate_configured(&config).unwrap();
        assert!(!preview.built);
        assert!(preview.rebuild.state.starts_with("waiting: estimated US$"));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn socket_is_owner_only_and_refuses_live_takeover() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_tempdir();
        let path = dir.path().join("memory-service.sock");
        let live = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            bind(&path).unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse
        );
        drop(live);
        drop(bind(&path).unwrap());
    }
}
