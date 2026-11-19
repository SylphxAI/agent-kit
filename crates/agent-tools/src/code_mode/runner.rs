//! Sandbox-side Programmatic Code Mode execution engines:
//! - [`CodeModeRunner::execute_batch`]: pure-Rust concurrent `Promise.all` batch execution
//! - [`CodeModeRunner::execute_js_stdio`]: host-free VM isolate over NDJSON stdio (Node / Bun / Deno)
//! - [`CodeModeRunner::serve_http_rpc`]: loopback/sandbox HTTP JSON-RPC 2.0 single & batch bridge
//! - [`CodeModeRunner::execute_in_sandbox`]: executes JS/TS strictly inside a remote [`WorkspaceBackend`]

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::SocketAddr,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinSet};

use crate::{
    code_mode::{
        contract::{CodeLimits, code_timeout_message, diagnose, truncate_json_output},
        protocol::{
            CodeExecRequest, CodeExecResult, JsonRpcPayload, JsonRpcResponse,
            JsonRpcResponsePayload, RpcError, ToolCallRequest, ToolCallResponse,
        },
        registry::ToolDispatcher,
        sdk::SANDBOX_WORKER_JS,
    },
    workspace::WorkspaceBackend,
};

/// Detected JavaScript/TypeScript sandbox runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsRuntimeKind {
    Bun,
    Node,
    Deno,
}

/// Detect an available sandbox JS/TS runtime (`bun`, `node`, or `deno`) on `PATH`.
pub fn detect_js_runtime() -> Result<(PathBuf, JsRuntimeKind), String> {
    let candidates: &[(&str, JsRuntimeKind)] = &[
        ("bun", JsRuntimeKind::Bun),
        ("node", JsRuntimeKind::Node),
        ("deno", JsRuntimeKind::Deno),
    ];
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for (bin, kind) in candidates {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                return Ok((candidate, *kind));
            }
        }
    }
    Err("no supported JS/TS runtime (`bun`, `node`, or `deno`) found on PATH".into())
}

/// Unified sandbox-side Programmatic Code Mode runner.
#[derive(Clone, Debug)]
pub struct CodeModeRunner {
    dispatcher: ToolDispatcher,
    limits: CodeLimits,
}

impl CodeModeRunner {
    pub fn new(dispatcher: ToolDispatcher) -> Self {
        Self {
            dispatcher,
            limits: CodeLimits::default(),
        }
    }

    pub fn with_limits(mut self, limits: CodeLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn dispatcher(&self) -> &ToolDispatcher {
        &self.dispatcher
    }

    pub fn limits(&self) -> CodeLimits {
        self.limits
    }

    /// Pure-Rust concurrent `Promise.all` batch executor.
    ///
    /// Executes all calls in `calls` concurrently within `timeout_ms`, preserves
    /// deterministic input ordering, isolates per-call failures, and bounds output
    /// serialization strictly to `max_output_bytes`.
    pub async fn execute_batch(
        &self,
        calls: Vec<ToolCallRequest>,
        timeout_ms: Option<u64>,
        max_output_bytes: Option<usize>,
        max_tool_calls: Option<usize>,
    ) -> CodeExecResult {
        let started = Instant::now();
        let effective_timeout = self.limits.resolve_timeout_ms(timeout_ms);
        let effective_output = self.limits.resolve_output_bytes(max_output_bytes);
        let effective_calls = self.limits.resolve_tool_calls(max_tool_calls);
        let counter = Arc::new(AtomicUsize::new(0));

        let fut = self
            .dispatcher
            .dispatch_batch(calls, effective_calls, Some(&counter));

        match tokio::time::timeout(Duration::from_millis(effective_timeout), fut).await {
            Ok(responses) => {
                let raw_val = serde_json::to_value(&responses).unwrap_or(Value::Null);
                let bounded = truncate_json_output(raw_val, effective_output);
                let all_ok = responses.iter().all(|r| r.ok);
                CodeExecResult {
                    ok: all_ok,
                    result: bounded.value,
                    logs: Vec::new(),
                    duration_ms: started.elapsed().as_millis() as u64,
                    truncated: bounded.truncated,
                    original_bytes: bounded.original_bytes,
                    tool_calls: counter.load(Ordering::SeqCst),
                    error: if all_ok {
                        None
                    } else {
                        Some("one or more batched tool calls failed".into())
                    },
                    fix: None,
                }
            }
            Err(_) => CodeExecResult {
                ok: false,
                result: Value::Null,
                logs: Vec::new(),
                duration_ms: started.elapsed().as_millis() as u64,
                truncated: false,
                original_bytes: 0,
                tool_calls: counter.load(Ordering::SeqCst),
                error: Some(code_timeout_message(
                    effective_timeout,
                    timeout_ms,
                    self.limits.max_ms,
                )),
                fix: None,
            },
        }
    }

    /// Execute a JS/TS snippet in a host-free sandboxed isolate over NDJSON stdio.
    ///
    /// - Clears child environment (`env_clear()`).
    /// - Scrubs `process`, `require`, `Bun`, `Deno`, `fetch`, and `WebSocket` inside VM.
    /// - Dispatches `Promise.all` batches concurrently through [`ToolDispatcher`].
    /// - Enforces wall-time timeout (killing the entire Unix process group on expiry),
    ///   max tool call cap, and bounded UTF-8 output truncation.
    pub async fn execute_js_stdio(&self, request: CodeExecRequest) -> CodeExecResult {
        let started = Instant::now();
        let timeout_ms = self.limits.resolve_timeout_ms(request.timeout_ms);
        let max_output_bytes = self.limits.resolve_output_bytes(request.max_output_bytes);
        let max_tool_calls = self.limits.resolve_tool_calls(request.max_tool_calls);

        let (runtime_bin, kind) = match detect_js_runtime() {
            Ok(rt) => rt,
            Err(err) => {
                return CodeExecResult {
                    ok: false,
                    result: Value::Null,
                    logs: Vec::new(),
                    duration_ms: 0,
                    truncated: false,
                    original_bytes: 0,
                    tool_calls: 0,
                    error: Some(err),
                    fix: None,
                };
            }
        };

        let mut cmd = Command::new(&runtime_bin);
        cmd.env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("NODE_NO_WARNINGS", "1");
        if let Some(home) = std::env::var_os("HOME") {
            cmd.env("HOME", home);
        }
        match kind {
            JsRuntimeKind::Node => {
                cmd.arg("--no-warnings").arg("-e").arg(SANDBOX_WORKER_JS);
            }
            JsRuntimeKind::Bun => {
                cmd.arg("-e").arg(SANDBOX_WORKER_JS);
            }
            JsRuntimeKind::Deno => {
                cmd.args(["eval", "--no-prompt", SANDBOX_WORKER_JS]);
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return CodeExecResult {
                    ok: false,
                    result: Value::Null,
                    logs: Vec::new(),
                    duration_ms: started.elapsed().as_millis() as u64,
                    truncated: false,
                    original_bytes: 0,
                    tool_calls: 0,
                    error: Some(format!(
                        "failed to spawn JS isolate ({}): {e}",
                        runtime_bin.display()
                    )),
                    fix: None,
                };
            }
        };

        let Some(mut child_stdin) = child.stdin.take() else {
            return error_result(started, "isolate stdin unavailable", 0);
        };
        let Some(child_stdout) = child.stdout.take() else {
            return error_result(started, "isolate stdout unavailable", 0);
        };
        let Some(child_stderr) = child.stderr.take() else {
            return error_result(started, "isolate stderr unavailable", 0);
        };
        let mut child_guard = ChildGroupGuard { child: Some(child) };

        let specs = self.dispatcher.specs();
        let start_frame = json!({
            "type": "start",
            "code": request.code,
            "namespaces": specs,
        });
        let Ok(mut line) = serde_json::to_vec(&start_frame) else {
            return error_result(started, "failed to serialize start frame", 0);
        };
        line.push(b'\n');
        if let Err(e) = child_stdin
            .write_all(&line)
            .and_then(|()| child_stdin.flush())
        {
            return error_result(started, &format!("failed sending code to isolate: {e}"), 0);
        }

        let shared_stdin = Arc::new(Mutex::new(child_stdin));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        std::thread::spawn(move || {
            let reader = BufReader::new(child_stdout);
            for line in reader.lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str::<Value>(trimmed)
                    && tx.send(val).is_err()
                {
                    break;
                }
            }
        });
        let stderr_handle = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buf = Vec::new();
            let _ = child_stderr.take(16 * 1024).read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        });

        let call_counter = Arc::new(AtomicUsize::new(0));
        let mut in_flight = JoinSet::new();
        let dispatcher = self.dispatcher.clone();

        let drive = async {
            while let Some(msg) = rx.recv().await {
                let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "batch" => {
                        let calls: Vec<ToolCallRequest> = msg
                            .get("calls")
                            .cloned()
                            .and_then(|v| serde_json::from_value(v).ok())
                            .unwrap_or_default();
                        let d = dispatcher.clone();
                        let counter = Arc::clone(&call_counter);
                        let stdin_ref = Arc::clone(&shared_stdin);
                        in_flight.spawn(async move {
                            let replies = d
                                .dispatch_batch(calls, max_tool_calls, Some(&counter))
                                .await;
                            let frame = json!({
                                "type": "batch_reply",
                                "replies": replies,
                            });
                            if let Ok(mut guard) = stdin_ref.lock() {
                                let _ = writeln!(guard, "{frame}");
                                let _ = guard.flush();
                            }
                        });
                    }
                    "call" => {
                        if let Ok(call) = serde_json::from_value::<ToolCallRequest>(msg) {
                            let d = dispatcher.clone();
                            let counter = Arc::clone(&call_counter);
                            let stdin_ref = Arc::clone(&shared_stdin);
                            in_flight.spawn(async move {
                                let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
                                let reply = if count > max_tool_calls {
                                    ToolCallResponse::err(
                                        call.id,
                                        RpcError::budget_exceeded(format!(
                                            "exceeded maximum of {max_tool_calls} tool calls"
                                        )),
                                    )
                                } else {
                                    d.dispatch_one(call).await
                                };
                                let frame = json!({
                                    "type": "reply",
                                    "id": reply.id,
                                    "ok": reply.ok,
                                    "result": reply.result,
                                    "error": reply.error,
                                });
                                if let Ok(mut guard) = stdin_ref.lock() {
                                    let _ = writeln!(guard, "{frame}");
                                    let _ = guard.flush();
                                }
                            });
                        }
                    }
                    "done" => return Ok(msg),
                    _ => {}
                }
            }
            Err("code isolate exited before returning a result".to_owned())
        };

        let outcome = tokio::time::timeout(Duration::from_millis(timeout_ms), drive).await;
        in_flight.abort_all();
        // Drop stdin before stopping child so well-behaved runtimes see EOF cleanly.
        drop(shared_stdin);

        let total_calls = call_counter.load(Ordering::SeqCst);
        let done_msg = match outcome {
            Ok(Ok(msg)) => {
                child_guard.stop();
                msg
            }
            Ok(Err(err)) => {
                child_guard.stop();
                let stderr = stderr_handle.join().unwrap_or_default();
                let full_err = if stderr.trim().is_empty() {
                    err
                } else {
                    format!("{err}: {}", stderr.trim())
                };
                return error_result(started, &full_err, total_calls);
            }
            Err(_) => {
                child_guard.stop();
                return error_result(
                    started,
                    &code_timeout_message(timeout_ms, request.timeout_ms, self.limits.max_ms),
                    total_calls,
                );
            }
        };

        let logs: Vec<String> = done_msg
            .get("logs")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        let ok = done_msg.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if !ok {
            let err = done_msg
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown code execution error")
                .to_owned();
            let compile = done_msg.get("phase").and_then(Value::as_str) == Some("compile");
            let fix = diagnose(&request.code, &err, compile, &specs);
            return CodeExecResult {
                ok: false,
                result: Value::Null,
                logs,
                duration_ms: started.elapsed().as_millis() as u64,
                truncated: false,
                original_bytes: 0,
                tool_calls: total_calls,
                error: Some(err),
                fix,
            };
        }

        let raw_result = done_msg.get("result").cloned().unwrap_or(Value::Null);
        let bounded = truncate_json_output(raw_result, max_output_bytes);
        CodeExecResult {
            ok: true,
            result: bounded.value,
            logs,
            duration_ms: started.elapsed().as_millis() as u64,
            truncated: bounded.truncated,
            original_bytes: bounded.original_bytes,
            tool_calls: total_calls,
            error: None,
            fix: None,
        }
    }

    /// Execute a JS/TS snippet strictly **inside** the target [`WorkspaceBackend`]
    /// (for Docker / RemoteSandbox backends, running the JS runtime inside the
    /// remote sandbox container rather than on the coordinator host).
    ///
    /// Note: this mode passes the snippet directly to the sandbox's runtime via
    /// `WorkspaceBackend::exec` against a sandbox-reachable `rpc_endpoint` (or
    /// pure compute if no RPC endpoint is supplied), never reading coordinator
    /// host files.
    pub async fn execute_in_sandbox(
        &self,
        backend: Arc<dyn WorkspaceBackend>,
        rpc_endpoint: Option<(String, String)>,
        request: CodeExecRequest,
    ) -> CodeExecResult {
        let started = Instant::now();
        let timeout_ms = self.limits.resolve_timeout_ms(request.timeout_ms);
        let max_output_bytes = self.limits.resolve_output_bytes(request.max_output_bytes);
        let specs = self.dispatcher.specs();
        let script = build_sandbox_exec_script(&request.code, rpc_endpoint.as_ref(), &specs);
        let encoded = crate::encoding::base64_encode(script.as_bytes());
        let cmd = format!(
            "RT=$(command -v bun || command -v node || command -v deno); \
             if [ -z \"$RT\" ]; then echo 'no js runtime in sandbox' >&2; exit 127; fi; \
             printf '%s' '{encoded}' | base64 -d | \"$RT\" -"
        );

        let res = tokio::task::spawn_blocking(move || backend.exec(&cmd, None, Some(timeout_ms)))
            .await
            .map_err(|e| format!("sandbox exec task failed: {e}"))
            .and_then(|r| r);

        match res {
            Ok(out) => {
                if out.exit_code != 0 {
                    let msg = if out.stderr.trim().is_empty() {
                        format!("sandbox runtime exited with status {}", out.exit_code)
                    } else {
                        out.stderr.trim().to_owned()
                    };
                    return error_result(started, &msg, 0);
                }
                let parsed: Value = serde_json::from_str(out.stdout.trim()).unwrap_or_else(
                    |_| json!({ "ok": true, "result": out.stdout.trim(), "logs": [] }),
                );
                let ok = parsed.get("ok").and_then(Value::as_bool).unwrap_or(true);
                let logs = parsed
                    .get("logs")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if !ok {
                    let err = parsed
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("sandbox execution error")
                        .to_owned();
                    return CodeExecResult {
                        ok: false,
                        result: Value::Null,
                        logs,
                        duration_ms: started.elapsed().as_millis() as u64,
                        truncated: false,
                        original_bytes: 0,
                        tool_calls: 0,
                        error: Some(err),
                        fix: None,
                    };
                }
                let raw_val = parsed.get("result").cloned().unwrap_or(Value::Null);
                let bounded = truncate_json_output(raw_val, max_output_bytes);
                CodeExecResult {
                    ok: true,
                    result: bounded.value,
                    logs,
                    duration_ms: started.elapsed().as_millis() as u64,
                    truncated: bounded.truncated,
                    original_bytes: bounded.original_bytes,
                    tool_calls: 0,
                    error: None,
                    fix: None,
                }
            }
            Err(err) => error_result(started, &err, 0),
        }
    }

    /// Start a sandbox loopback HTTP JSON-RPC 2.0 server (`POST /rpc`) authenticated
    /// with a per-session bearer token, supporting both single calls and concurrent
    /// `Promise.all` JSON-RPC 2.0 array batches.
    pub async fn serve_http_rpc(&self, bind_addr: SocketAddr) -> Result<RpcHttpServer, String> {
        let token = format!("rpc_{}", uuid::Uuid::now_v7());
        let state = Arc::new(HttpRpcState {
            dispatcher: self.dispatcher.clone(),
            token: token.clone(),
            max_batch_calls: self.limits.max_tool_calls,
        });
        let app = Router::new()
            .route("/rpc", post(handle_http_rpc))
            .with_state(state);
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(|e| format!("failed to bind sandbox HTTP RPC on {bind_addr}: {e}"))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("failed reading bound socket addr: {e}"))?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        Ok(RpcHttpServer {
            addr: local_addr,
            token,
            shutdown: Some(shutdown_tx),
        })
    }
}

/// Running loopback/sandbox HTTP JSON-RPC 2.0 server handle.
#[derive(Debug)]
pub struct RpcHttpServer {
    addr: SocketAddr,
    token: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl RpcHttpServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn endpoint_url(&self) -> String {
        format!("http://{}/rpc", self.addr)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for RpcHttpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct HttpRpcState {
    dispatcher: ToolDispatcher,
    token: String,
    max_batch_calls: usize,
}

async fn handle_http_rpc(
    State(state): State<Arc<HttpRpcState>>,
    headers: HeaderMap,
    Json(payload): Json<JsonRpcPayload>,
) -> Result<Json<JsonRpcResponsePayload>, (StatusCode, String)> {
    let expected = format!("Bearer {}", state.token);
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth != expected {
        return Err((StatusCode::UNAUTHORIZED, "invalid bearer token".into()));
    }

    match payload {
        JsonRpcPayload::Single(req) => {
            let resp = match ToolCallRequest::from_compound(req.id.clone(), &req.method, req.params)
            {
                Ok(call) => state.dispatcher.dispatch_one(call).await,
                Err(err) => ToolCallResponse::err(req.id, err),
            };
            Ok(Json(JsonRpcResponsePayload::Single(resp.into())))
        }
        JsonRpcPayload::Batch(reqs) => {
            if reqs.len() > state.max_batch_calls {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "batch size {} exceeds limit {}",
                        reqs.len(),
                        state.max_batch_calls
                    ),
                ));
            }
            let mut valid_calls = Vec::new();
            let mut slot_map: Vec<Result<usize, ToolCallResponse>> = Vec::with_capacity(reqs.len());
            for req in reqs {
                match ToolCallRequest::from_compound(req.id.clone(), &req.method, req.params) {
                    Ok(call) => {
                        slot_map.push(Ok(valid_calls.len()));
                        valid_calls.push(call);
                    }
                    Err(err) => slot_map.push(Err(ToolCallResponse::err(req.id, err))),
                }
            }
            let dispatched = state
                .dispatcher
                .dispatch_batch(valid_calls, state.max_batch_calls, None)
                .await;
            let mut dispatched_opts: Vec<Option<ToolCallResponse>> =
                dispatched.into_iter().map(Some).collect();
            let responses: Vec<JsonRpcResponse> = slot_map
                .into_iter()
                .map(|slot| match slot {
                    Ok(i) => dispatched_opts[i].take().expect("unique batch slot").into(),
                    Err(err_resp) => err_resp.into(),
                })
                .collect();
            Ok(Json(JsonRpcResponsePayload::Batch(responses)))
        }
    }
}

fn build_sandbox_exec_script(
    user_code: &str,
    rpc: Option<&(String, String)>,
    specs: &[crate::code_mode::registry::NamespaceSpec],
) -> String {
    let endpoint_json =
        serde_json::to_string(&rpc.map(|(u, _)| u.as_str())).unwrap_or("null".into());
    let token_json = serde_json::to_string(&rpc.map(|(_, t)| t.as_str())).unwrap_or("null".into());
    let specs_json = serde_json::to_string(specs).unwrap_or("[]".into());
    let code_json = serde_json::to_string(user_code).unwrap_or("\"\"".into());
    format!(
        r#"
(async () => {{
  const endpoint = {endpoint_json};
  const token = {token_json};
  const specs = {specs_json};
  const logs = [];
  let nextId = 1;
  let queue = [];
  let scheduled = false;

  async function flush() {{
    scheduled = false;
    const batch = queue;
    queue = [];
    if (batch.length === 0) return;
    if (!endpoint) {{
      for (const item of batch) item.reject(new Error("no sandbox RPC endpoint configured"));
      return;
    }}
    const payload = batch.length === 1
      ? {{ jsonrpc: "2.0", id: batch[0].id, method: batch[0].method, params: batch[0].params }}
      : batch.map((c) => ({{ jsonrpc: "2.0", id: c.id, method: c.method, params: c.params }}));
    try {{
      const resp = await fetch(endpoint, {{
        method: "POST",
        headers: {{
          "content-type": "application/json",
          ...(token ? {{ authorization: `Bearer ${{token}}` }} : {{}}),
        }},
        body: JSON.stringify(payload),
      }});
      const data = await resp.json();
      const replies = Array.isArray(data) ? data : [data];
      const byId = new Map(replies.map((r) => [r.id, r]));
      for (const item of batch) {{
        const reply = byId.get(item.id);
        if (!reply) item.reject(new Error(`missing reply for id ${{item.id}}`));
        else if (reply.error) item.reject(new Error(reply.error.message || JSON.stringify(reply.error)));
        else item.resolve(reply.result);
      }}
    }} catch (err) {{
      for (const item of batch) item.reject(err);
    }}
  }}

  const invoke = (ns, method, params) => new Promise((resolve, reject) => {{
    const id = nextId++;
    queue.push({{ id, method: `${{ns}}.${{method}}`, params: params ?? {{}}, resolve, reject }});
    if (!scheduled) {{
      scheduled = true;
      queueMicrotask(flush);
    }}
  }});

  const tools = {{ call: invoke }};
  for (const spec of specs) {{
    const nsObj = {{}};
    for (const m of spec.methods) {{
      nsObj[m] = (params) => invoke(spec.namespace, m, params ?? {{}});
    }}
    tools[spec.namespace] = Object.freeze(nsObj);
    globalThis[spec.namespace] = tools[spec.namespace];
  }}
  globalThis.tools = Object.freeze(tools);
  const userConsole = {{
    log: (...a) => logs.push(a.map(String).join(" ")),
    info: (...a) => logs.push("[info] " + a.map(String).join(" ")),
    warn: (...a) => logs.push("[warn] " + a.map(String).join(" ")),
    error: (...a) => logs.push("[error] " + a.map(String).join(" ")),
  }};

  try {{
    const fn = new Function("tools", "console", "return (async () => {{\n" + {code_json} + "\n}})();");
    const result = await fn(globalThis.tools, userConsole);
    process.stdout.write(JSON.stringify({{ ok: true, result: result === undefined ? null : result, logs }}) + "\n");
  }} catch (err) {{
    process.stdout.write(JSON.stringify({{ ok: false, error: err && err.stack ? String(err.stack) : String(err), logs }}) + "\n");
  }}
}})();
"#
    )
}

fn error_result(started: Instant, message: &str, tool_calls: usize) -> CodeExecResult {
    CodeExecResult {
        ok: false,
        result: Value::Null,
        logs: Vec::new(),
        duration_ms: started.elapsed().as_millis() as u64,
        truncated: false,
        original_bytes: 0,
        tool_calls,
        error: Some(message.to_owned()),
        fix: None,
    }
}

struct ChildGroupGuard {
    child: Option<Child>,
}

impl ChildGroupGuard {
    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        {
            if let Ok(pgid) = i32::try_from(child.id())
                && pgid > 0
            {
                // SAFETY: sending signals to the process group created for this child.
                unsafe {
                    libc::kill(-pgid, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_millis(250);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for ChildGroupGuard {
    fn drop(&mut self) {
        self.stop();
    }
}
