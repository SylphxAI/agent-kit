//! End-to-end integration tests for Sandbox-side Programmatic Code Mode:
//! - Concurrent `Promise.all` batching with deterministic ordering & partial error isolation
//! - Host-free isolation via virtual remote `WorkspaceBackend` (zero coordinator host FS access)
//! - Fail-closed handling of unknown namespaces and unknown methods
//! - Timeout enforcement, call budget caps, and UTF-8-safe bounded output truncation
//! - Sandbox loopback HTTP JSON-RPC 2.0 single & batch bridge with bearer authentication

use agent_tools::{
    ExecOutput, WorkspaceBackend,
    code_mode::{
        CodeExecRequest, CodeLimits, CodeModeRunner, FnNamespaceHandler, ToolCallRequest,
        ToolError, ToolRegistry, detect_js_runtime,
        protocol::{ERR_METHOD_NOT_FOUND, ToolCallResponse},
    },
    sha256_hex,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

struct VirtualRemoteWorkspace {
    root: PathBuf,
    files: Mutex<HashMap<String, Vec<u8>>>,
}

impl VirtualRemoteWorkspace {
    fn new(root: &str, seed: &[(&str, &[u8])]) -> Self {
        let mut map = HashMap::new();
        for (k, v) in seed {
            let full = format!(
                "{}/{}",
                root.trim_end_matches('/'),
                k.trim_start_matches('/')
            );
            map.insert(full, v.to_vec());
        }
        Self {
            root: PathBuf::from(root),
            files: Mutex::new(map),
        }
    }
}

impl WorkspaceBackend for VirtualRemoteWorkspace {
    fn kind(&self) -> &'static str {
        "virtual-sandbox"
    }

    fn base_path(&self) -> &Path {
        &self.root
    }

    fn exec(
        &self,
        command: &str,
        _cwd: Option<&str>,
        _timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        Ok(ExecOutput {
            stdout: format!("sandbox-exec:{command}"),
            stderr: String::new(),
            exit_code: 0,
        })
    }

    fn read_file_bytes(&self, path: Option<&str>) -> Result<(String, Vec<u8>), String> {
        let resolved = self.resolve_path(path, true)?;
        let bytes = self
            .files
            .lock()
            .unwrap()
            .get(&resolved)
            .cloned()
            .ok_or_else(|| format!("virtual sandbox file not found: {resolved}"))?;
        Ok((resolved, bytes))
    }

    fn write_file_bytes(
        &self,
        path: Option<&str>,
        content: &[u8],
        append: bool,
        overwrite: bool,
    ) -> Result<(String, usize, String), String> {
        let resolved = self.resolve_path(path, true)?;
        let mut guard = self.files.lock().unwrap();
        if !append && !overwrite && guard.contains_key(&resolved) {
            return Err(format!("file {resolved} already exists"));
        }
        let entry = guard.entry(resolved.clone()).or_default();
        if append {
            entry.extend_from_slice(content);
        } else {
            *entry = content.to_vec();
        }
        let len = entry.len();
        let sha = sha256_hex(entry);
        Ok((resolved, len, sha))
    }

    fn list_dir(
        &self,
        path: Option<&str>,
        _include_hidden: bool,
        _max_results: usize,
    ) -> Result<Value, String> {
        let resolved = self.resolve_path(path, false)?;
        let prefix = format!("{}/", resolved.trim_end_matches('/'));
        let entries: Vec<Value> = self
            .files
            .lock()
            .unwrap()
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix))
            .filter(|rel| !rel.contains('/'))
            .map(|name| json!({ "name": name, "path": name, "kind": "file", "bytes": 0 }))
            .collect();
        let count = entries.len();
        Ok(json!({
            "action": "list",
            "path": resolved,
            "entries": entries,
            "count": count,
            "truncated": false
        }))
    }
}

fn build_multi_namespace_registry(ws: Arc<dyn WorkspaceBackend>) -> ToolRegistry {
    let mut reg = ToolRegistry::new().with_workspace(ws).with_default_web();

    // Caller-mounted `memory` namespace.
    reg.mount(Arc::new(FnNamespaceHandler::new(
        "memory",
        &["recall", "status"],
        |method, params| async move {
            match method.as_str() {
                "recall" => Ok(json!({
                    "query": params.get("query").cloned().unwrap_or(Value::Null),
                    "hits": [{ "document_id": "doc-1", "score": 0.98 }]
                })),
                "status" => Ok(json!({ "documents": 42 })),
                _ => Err(ToolError::Execution("unexpected".into())),
            }
        },
    )));

    // Caller-mounted `workstation` namespace.
    reg.mount(Arc::new(FnNamespaceHandler::new(
        "workstation",
        &["snapshot"],
        |_method, _params| async move { Ok(json!({ "refs": ["@e1", "@e2"] })) },
    )));

    // Extensible custom orchestrator namespace (`coordination`) — zero hardcoding in crate.
    reg.mount(Arc::new(FnNamespaceHandler::new(
        "coordination",
        &["ping", "slow", "fail"],
        |method, params| async move {
            match method.as_str() {
                "ping" => {
                    let delay = params.get("delay_ms").and_then(Value::as_u64).unwrap_or(0);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    Ok(json!({ "pong": params.get("seq").cloned().unwrap_or(json!(0)) }))
                }
                "slow" => {
                    let ms = params.get("ms").and_then(Value::as_u64).unwrap_or(5_000);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok(json!({ "slept_ms": ms }))
                }
                "fail" => Err(ToolError::Execution("intentional failure".into())),
                _ => Err(ToolError::Execution("unexpected".into())),
            }
        },
    )));

    reg
}

#[tokio::test]
async fn pure_rust_batch_runs_concurrently_preserves_order_and_isolates_errors() {
    let ws = Arc::new(VirtualRemoteWorkspace::new(
        "/sandbox",
        &[("a.txt", b"alpha"), ("b.txt", b"beta")],
    ));
    let runner = CodeModeRunner::new(build_multi_namespace_registry(ws).into_dispatcher());

    // Three staggered delays (120ms, 10ms, 80ms) plus one unknown namespace and one failing call.
    // Concurrent execution completes near ~120ms (well below serial sum 210ms) and preserves order.
    let started = Instant::now();
    let res = runner
        .execute_batch(
            vec![
                ToolCallRequest::new(
                    1u64,
                    "coordination",
                    "ping",
                    json!({ "seq": 1, "delay_ms": 120 }),
                ),
                ToolCallRequest::new(2u64, "workspace", "read", json!({ "path": "a.txt" })),
                ToolCallRequest::new(
                    3u64,
                    "coordination",
                    "ping",
                    json!({ "seq": 3, "delay_ms": 80 }),
                ),
                ToolCallRequest::new(4u64, "unknown_ns", "noop", json!({})),
                ToolCallRequest::new(5u64, "coordination", "fail", json!({})),
                ToolCallRequest::new(6u64, "memory", "status", json!({})),
                ToolCallRequest::new(7u64, "workstation", "snapshot", json!({})),
            ],
            Some(5_000),
            None,
            None,
        )
        .await;
    let elapsed = started.elapsed();

    assert!(!res.ok, "batch contains intentional failures");
    assert_eq!(res.tool_calls, 7);
    assert!(
        elapsed < Duration::from_millis(190),
        "expected concurrent execution (<190ms), took {elapsed:?}"
    );

    let replies: Vec<ToolCallResponse> = serde_json::from_value(res.result).unwrap();
    assert_eq!(replies.len(), 7);
    assert_eq!(replies[0].id, 1u64.into());
    assert_eq!(replies[0].result.as_ref().unwrap()["pong"], 1);
    assert_eq!(replies[1].result.as_ref().unwrap()["content"], "alpha");
    assert_eq!(replies[2].result.as_ref().unwrap()["pong"], 3);
    assert!(!replies[3].ok);
    assert_eq!(
        replies[3].error.as_ref().unwrap().code,
        ERR_METHOD_NOT_FOUND
    );
    assert!(
        replies[3]
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("unknown namespace `unknown_ns`")
    );
    assert!(!replies[4].ok);
    assert!(
        replies[4]
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("intentional failure")
    );
    assert_eq!(replies[5].result.as_ref().unwrap()["documents"], 42);
    assert_eq!(
        replies[6].result.as_ref().unwrap()["refs"],
        json!(["@e1", "@e2"])
    );
}

#[tokio::test]
async fn unknown_method_timeout_and_oversized_output_bounds_hold() {
    let ws = Arc::new(VirtualRemoteWorkspace::new("/sandbox", &[]));
    let runner = CodeModeRunner::new(build_multi_namespace_registry(ws).into_dispatcher());

    // Unknown method inside known namespace fails closed.
    let unknown_method = runner
        .execute_batch(
            vec![ToolCallRequest::new(
                1u64,
                "workspace",
                "delete_host",
                json!({}),
            )],
            None,
            None,
            None,
        )
        .await;
    assert!(!unknown_method.ok);
    let replies: Vec<ToolCallResponse> = serde_json::from_value(unknown_method.result).unwrap();
    assert_eq!(
        replies[0].error.as_ref().unwrap().code,
        ERR_METHOD_NOT_FOUND
    );

    // Timeout enforcement cancels slow call promptly.
    let started = Instant::now();
    let timed_out = runner
        .execute_batch(
            vec![ToolCallRequest::new(
                1u64,
                "coordination",
                "slow",
                json!({ "ms": 5_000 }),
            )],
            Some(150),
            None,
            None,
        )
        .await;
    assert!(!timed_out.ok);
    assert!(
        timed_out
            .error
            .as_deref()
            .unwrap_or("")
            .contains("timed out")
    );
    assert!(started.elapsed() < Duration::from_secs(2));

    // Oversized output truncation holds.
    let big_ws = Arc::new(VirtualRemoteWorkspace::new(
        "/sandbox",
        &[("big.txt", "x".repeat(40_000).as_bytes())],
    ));
    let big_runner = CodeModeRunner::new(build_multi_namespace_registry(big_ws).into_dispatcher());
    let bounded = big_runner
        .execute_batch(
            vec![ToolCallRequest::new(
                1u64,
                "workspace",
                "read",
                json!({ "path": "big.txt" }),
            )],
            None,
            Some(512),
            None,
        )
        .await;
    assert!(bounded.ok);
    assert!(bounded.truncated);
    assert!(bounded.original_bytes > 30_000);
    assert_eq!(bounded.result["truncated"], true);
}

#[tokio::test]
async fn http_jsonrpc_bridge_authenticates_and_executes_concurrent_batches() {
    let ws = Arc::new(VirtualRemoteWorkspace::new(
        "/sandbox",
        &[("hello.txt", b"sandbox-only")],
    ));
    let runner = CodeModeRunner::new(build_multi_namespace_registry(ws).into_dispatcher());
    let mut server = runner
        .serve_http_rpc("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("start loopback RPC");

    let endpoint = server.endpoint_url();
    let token = server.token().to_owned();

    // Unauthenticated call must be rejected with 401.
    let unauth_status = tokio::task::spawn_blocking({
        let endpoint = endpoint.clone();
        move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into();
            agent
                .post(&endpoint)
                .send_json(json!({"jsonrpc":"2.0","id":1,"method":"memory.status","params":{}}))
                .unwrap()
                .status()
                .as_u16()
        }
    })
    .await
    .unwrap();
    assert_eq!(unauth_status, 401);

    // Authorized JSON-RPC 2.0 array batch executes concurrently and returns ordered replies.
    let batch_reply: Value = tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent
            .post(&endpoint)
            .header("authorization", &format!("Bearer {token}"))
            .send_json(json!([
                { "jsonrpc": "2.0", "id": "first", "method": "workspace.read", "params": { "path": "hello.txt" } },
                { "jsonrpc": "2.0", "id": "second", "method": "coordination.ping", "params": { "seq": 99 } },
                { "jsonrpc": "2.0", "id": "third", "method": "nope.missing", "params": {} }
            ]))
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        resp.body_mut().read_json::<Value>().unwrap()
    })
    .await
    .unwrap();

    let arr = batch_reply.as_array().expect("batch reply array");
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["id"], "first");
    assert_eq!(arr[0]["result"]["content"], "sandbox-only");
    assert_eq!(arr[1]["id"], "second");
    assert_eq!(arr[1]["result"]["pong"], 99);
    assert_eq!(arr[2]["id"], "third");
    assert_eq!(arr[2]["error"]["code"], ERR_METHOD_NOT_FOUND);

    server.shutdown();
}

#[tokio::test]
async fn js_stdio_isolate_proves_promise_all_host_free_isolation_timeout_and_truncation() {
    if detect_js_runtime().is_err() {
        return;
    }

    // Create a real file on the coordinator host that MUST NOT be readable by the sandboxed workspace.
    let host_dir = tempfile::tempdir().unwrap();
    let host_secret = host_dir.path().join("coordinator-secret.txt");
    std::fs::write(&host_secret, b"TOP_SECRET_COORDINATOR_HOST_DATA").unwrap();

    let virtual_ws = Arc::new(VirtualRemoteWorkspace::new(
        "/sandbox",
        &[("a.txt", b"from-sandbox-a"), ("b.txt", b"from-sandbox-b")],
    ));
    let runner = CodeModeRunner::new(build_multi_namespace_registry(virtual_ws).into_dispatcher())
        .with_limits(CodeLimits {
            default_ms: 10_000,
            max_ms: 30_000,
            max_output_bytes: 64 * 1024,
            max_tool_calls: 16,
        });

    // 1. Concurrent Promise.all across workspace + memory + workstation + custom coordination + TS types.
    let host_secret_str = host_secret.display().to_string();
    let code = format!(
        r#"
        const hostGlobals = {{
          process: typeof process,
          require: typeof require,
          fetch: typeof fetch,
          Bun: typeof Bun,
          Deno: typeof Deno,
        }};
        const [a, b, mem, wsSnap, ping] = await Promise.all([
          workspace.read("a.txt"),
          workspace.read("b.txt"),
          memory.recall({{ query: "deploy" }}),
          workstation.snapshot({{}}),
          coordination.ping({{ seq: 7, delay_ms: 40 }}),
        ]);
        let hostReadBlocked = false;
        try {{
          await workspace.read({host_secret_json});
        }} catch (e) {{
          hostReadBlocked = true;
        }}
        let unknownBlocked = false;
        try {{
          await tools.call("nonexistent", "op", {{}});
        }} catch (e) {{
          unknownBlocked = String(e).includes("unknown namespace");
        }}
        console.log("done-batch");
        return {{
          hostGlobals,
          a: a.content,
          b: b.content,
          memHit: mem.hits[0].document_id,
          wsRefs: wsSnap.refs,
          pong: ping.pong,
          hostReadBlocked,
          unknownBlocked,
        }};
        "#,
        host_secret_json = serde_json::to_string(&host_secret_str).unwrap()
    );

    let res = runner
        .execute_js_stdio(CodeExecRequest {
            code,
            timeout_ms: Some(10_000),
            max_output_bytes: None,
            max_tool_calls: None,
        })
        .await;

    assert!(
        res.ok,
        "expected ok, got error: {:?} fix: {:?}",
        res.error, res.fix
    );
    assert_eq!(res.result["a"], "from-sandbox-a");
    assert_eq!(res.result["b"], "from-sandbox-b");
    assert_eq!(res.result["memHit"], "doc-1");
    assert_eq!(res.result["wsRefs"], json!(["@e1", "@e2"]));
    assert_eq!(res.result["pong"], 7);
    assert_eq!(res.result["hostReadBlocked"], true);
    assert_eq!(res.result["unknownBlocked"], true);
    assert_eq!(res.result["hostGlobals"]["process"], "undefined");
    assert_eq!(res.result["hostGlobals"]["require"], "undefined");
    assert_eq!(res.result["hostGlobals"]["fetch"], "undefined");
    assert_eq!(res.logs, vec!["done-batch"]);
    assert_eq!(res.tool_calls, 7);

    // 2. Oversized output truncation holds in JS mode.
    let big = runner
        .execute_js_stdio(CodeExecRequest {
            code: r#"return { payload: "z".repeat(20000) };"#.into(),
            timeout_ms: Some(5_000),
            max_output_bytes: Some(512),
            max_tool_calls: None,
        })
        .await;
    assert!(big.ok);
    assert!(big.truncated);
    assert!(big.original_bytes > 20_000);
    assert_eq!(big.result["truncated"], true);

    // 3. Timeout kills the isolate promptly.
    let started = Instant::now();
    let timed_out = runner
        .execute_js_stdio(CodeExecRequest {
            code: r#"await new Promise((r) => setTimeout(r, 10000)); return "never";"#.into(),
            timeout_ms: Some(250),
            max_output_bytes: None,
            max_tool_calls: None,
        })
        .await;
    assert!(!timed_out.ok);
    assert!(
        timed_out
            .error
            .as_deref()
            .unwrap_or("")
            .contains("timed out")
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn js_console_keeps_more_than_200_records_and_long_unicode_inline() {
    if detect_js_runtime().is_err() {
        return;
    }
    let runner = CodeModeRunner::new(ToolRegistry::new().into_dispatcher());
    let res = runner
        .execute_js_stdio(CodeExecRequest {
            code: r#"
              for (let i = 0; i < 225; i++) console.log(`record-${i}`);
              console.debug("界🙂".repeat(2100) + "\n\0end");
              return 42;
            "#
            .into(),
            ..CodeExecRequest::default()
        })
        .await;
    assert!(res.ok, "{res:?}");
    let mut expected: Vec<_> = (0..225).map(|i| format!("record-{i}")).collect();
    expected.push(format!("[debug] {}\n\0end", "界🙂".repeat(2100)));
    assert_eq!(res.logs, expected);
    assert!(res.log_artifact.is_none());
    assert!(!res.logs_incomplete);
    assert_eq!(res.result, 42);
}

#[test]
fn cli_console_overflow_recovers_every_record_after_snippet_error() {
    if detect_js_runtime().is_err() {
        return;
    }
    let workdir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_agent-code-mode"))
        .args(["run", "--max-output-bytes", "128", "--workdir"])
        .arg(workdir.path())
        .args([
            "--code",
            r#"
              console.log("starts inline");
              console.warn("界🙂".repeat(2300) + "\n\0tail");
              for (let i = 0; i < 220; i++) console.info(`record-${i}`);
              console.error("terminal");
              throw new Error("snippet failed");
            "#,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let res: agent_tools::code_mode::CodeExecResult = serde_json::from_slice(&out.stdout).unwrap();
    assert!(res.error.unwrap().contains("snippet failed"));
    assert!(!res.logs_incomplete);
    assert_eq!(res.logs, ["starts inline"]);
    assert!(serde_json::to_vec(&res.logs).unwrap().len() <= 128);
    let artifact = res
        .log_artifact
        .expect("overflow must be independently retrievable");
    let raw = std::fs::read_to_string(&artifact.path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&artifact.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }
    std::fs::remove_file(&artifact.path).unwrap();
    assert_eq!(artifact.entries, 223);
    assert_eq!(artifact.bytes, raw.len() as u64);
    let captured: Vec<String> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut expected = vec![
        "starts inline".into(),
        format!("[warn] {}\n\0tail", "界🙂".repeat(2300)),
    ];
    expected.extend((0..220).map(|i| format!("[info] record-{i}")));
    expected.push("[error] terminal".into());
    assert_eq!(captured, expected);
}

#[tokio::test]
async fn js_console_keeps_completed_records_on_timeout() {
    if detect_js_runtime().is_err() {
        return;
    }
    let runner = CodeModeRunner::new(ToolRegistry::new().into_dispatcher());
    let res = runner
        .execute_js_stdio(CodeExecRequest {
            code:
                r#"console.log("before timeout"); await new Promise((r) => setTimeout(r, 120000));"#
                    .into(),
            timeout_ms: Some(1000),
            ..CodeExecRequest::default()
        })
        .await;
    assert!(!res.ok);
    assert!(res.logs_incomplete);
    assert_eq!(res.logs, ["before timeout"]);
    assert!(res.log_artifact.is_none());
}
