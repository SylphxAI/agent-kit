//! Unified Sandbox-side Programmatic Code Mode SDK, JSON-RPC / NDJSON bridge,
//! extensible namespace registry, and host-free isolate runner.
//!
//! ## Architecture
//! - **Host-free isolation**: Code runs strictly inside the agent sandbox or a
//!   locked-down VM isolate with cleared environment and scrubbed Node/Bun/Deno
//!   host globals (`process`, `require`, `fetch`, `Bun`, `Deno`, `WebSocket`).
//!   Workspace operations route exclusively through [`crate::WorkspaceBackend`],
//!   never ambient coordinator host paths.
//! - **Concurrent `Promise.all` batching**: Both the NDJSON stdio bridge and the
//!   sandbox loopback HTTP JSON-RPC 2.0 server dispatch batched and pipelined
//!   tool calls concurrently while returning deterministic ordered replies.
//! - **Zero orchestrator hardcoding**: Standard namespaces (`workspace`, `web`,
//!   plus caller-wired `memory` and `workstation`) and arbitrary caller-mounted
//!   namespaces (such as `coordination`) all implement [`NamespaceHandler`] and
//!   register into [`ToolRegistry`].

mod console;
pub mod contract;
pub mod protocol;
pub mod registry;
pub mod runner;
pub mod sdk;

pub use contract::{CodeLimits, TruncatedOutput, diagnose, truncate_json_output};
pub use protocol::{
    CodeExecRequest, CodeExecResult, ConsoleLogArtifact, JsonRpcRequest, JsonRpcResponse, RpcError,
    RpcId, ToolCallRequest, ToolCallResponse,
};
pub use registry::{
    FnNamespaceHandler, NamespaceHandler, NamespaceSpec, ToolDispatcher, ToolError, ToolRegistry,
    WebNamespaceHandler, WorkspaceNamespaceHandler,
};
pub use runner::{CodeModeRunner, JsRuntimeKind, RpcHttpServer, detect_js_runtime};
pub use sdk::{SANDBOX_HTTP_CLIENT_JS, SANDBOX_WORKER_JS, generate_ts_declarations};
