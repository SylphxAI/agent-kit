//! Thin sandbox-side CLI runner for programmatic Code Mode (`agent-code-mode`).
//!
//! Subcommands:
//! - `run --workdir <DIR> [--timeout-ms N] [--max-output-bytes N] [--code <TS> | --stdin]`
//! - `rpc-stdio --workdir <DIR>` (NDJSON JSON-RPC 2.0 single & batch dispatcher over stdin/stdout)
//! - `serve-rpc --workdir <DIR> [--listen 127.0.0.1:0]` (sandbox loopback HTTP JSON-RPC 2.0 bridge)
//! - `types` (prints TypeScript `.d.ts` declarations for default mounted namespaces)

use agent_tools::{
    LocalWorkspace,
    code_mode::{
        CodeExecRequest, CodeLimits, CodeModeRunner, ToolCallRequest, ToolCallResponse,
        ToolRegistry, generate_ts_declarations,
        protocol::{JsonRpcPayload, JsonRpcResponse, JsonRpcResponsePayload},
    },
};
use std::{
    io::{self, BufRead, Read, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let workdir = PathBuf::from(arg("--workdir").unwrap_or_else(|| ".".into()));
    let backend = Arc::new(LocalWorkspace::new(workdir));
    let registry = ToolRegistry::new()
        .with_workspace(backend)
        .with_default_web();

    match args.first().map(String::as_str) {
        Some("types") => {
            print!("{}", generate_ts_declarations(&registry));
            Ok(())
        }
        Some("run") => {
            let code = if let Some(inline) = arg("--code") {
                inline
            } else {
                let mut buf = String::new();
                io::stdin().read_to_string(&mut buf)?;
                buf
            };
            let timeout_ms = arg("--timeout-ms").map(|v| v.parse::<u64>()).transpose()?;
            let max_output_bytes = arg("--max-output-bytes")
                .map(|v| v.parse::<usize>())
                .transpose()?;
            let runner =
                CodeModeRunner::new(registry.into_dispatcher()).with_limits(CodeLimits::from_env());
            let result = runner
                .execute_js_stdio(CodeExecRequest {
                    code,
                    timeout_ms,
                    max_output_bytes,
                    max_tool_calls: None,
                })
                .await;
            println!("{}", serde_json::to_string_pretty(&result)?);
            if !result.ok {
                std::process::exit(1);
            }
            Ok(())
        }
        Some("rpc-stdio") => {
            let dispatcher = registry.into_dispatcher();
            let limits = CodeLimits::from_env();
            let stdin = io::stdin();
            let mut stdout = io::stdout();
            for line in stdin.lock().lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let payload: JsonRpcPayload = serde_json::from_str(trimmed)?;
                let response_payload = match payload {
                    JsonRpcPayload::Single(req) => {
                        let resp = match ToolCallRequest::from_compound(
                            req.id.clone(),
                            &req.method,
                            req.params,
                        ) {
                            Ok(call) => dispatcher.dispatch_one(call).await,
                            Err(err) => ToolCallResponse::err(req.id, err),
                        };
                        JsonRpcResponsePayload::Single(resp.into())
                    }
                    JsonRpcPayload::Batch(reqs) => {
                        let mut calls = Vec::new();
                        let mut slots = Vec::with_capacity(reqs.len());
                        for req in reqs {
                            match ToolCallRequest::from_compound(
                                req.id.clone(),
                                &req.method,
                                req.params,
                            ) {
                                Ok(c) => {
                                    slots.push(Ok(calls.len()));
                                    calls.push(c);
                                }
                                Err(err) => slots.push(Err(ToolCallResponse::err(req.id, err))),
                            }
                        }
                        let mut done: Vec<Option<ToolCallResponse>> = dispatcher
                            .dispatch_batch(calls, limits.max_tool_calls, None)
                            .await
                            .into_iter()
                            .map(Some)
                            .collect();
                        let responses: Vec<JsonRpcResponse> = slots
                            .into_iter()
                            .map(|slot| match slot {
                                Ok(i) => done[i].take().expect("batch slot").into(),
                                Err(r) => r.into(),
                            })
                            .collect();
                        JsonRpcResponsePayload::Batch(responses)
                    }
                };
                writeln!(stdout, "{}", serde_json::to_string(&response_payload)?)?;
                stdout.flush()?;
            }
            Ok(())
        }
        Some("serve-rpc") => {
            let listen: SocketAddr = arg("--listen")
                .unwrap_or_else(|| "127.0.0.1:0".into())
                .parse()?;
            let runner =
                CodeModeRunner::new(registry.into_dispatcher()).with_limits(CodeLimits::from_env());
            let server = runner.serve_http_rpc(listen).await?;
            println!(
                "{}",
                serde_json::json!({
                    "endpoint": server.endpoint_url(),
                    "token": server.token(),
                })
            );
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        _ => {
            eprintln!("usage: agent-code-mode <run|rpc-stdio|serve-rpc|types> [--workdir DIR] ...");
            std::process::exit(2);
        }
    }
}
