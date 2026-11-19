//! Extensible tool namespace registry and concurrent batch dispatcher.
//!
//! Callers mount standard namespaces (`workspace`, `web`, `memory`, `workstation`)
//! and any custom orchestrator namespace (such as `coordination`) using
//! [`NamespaceHandler`] — zero orchestrator hardcoding lives in this crate.

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::{
    code_mode::protocol::{RpcError, ToolCallRequest, ToolCallResponse},
    ops::{
        self, EditParams, ExecParams, GlobParams, GrepParams, ListParams, MultiEditParams,
        ReadParams, WriteParams,
    },
    web::{DefaultWebBackend, WebBackend, WebFetchRequest, WebSearchRequest},
    workspace::WorkspaceBackend,
};

/// Typed error returned by a [`NamespaceHandler`].
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown method `{namespace}.{method}`; available: {available:?}")]
    UnknownMethod {
        namespace: String,
        method: String,
        available: Vec<String>,
    },
    #[error("invalid parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    Execution(String),
}

impl From<ToolError> for RpcError {
    fn from(err: ToolError) -> Self {
        match err {
            ToolError::UnknownMethod { .. } => RpcError::method_not_found(err.to_string()),
            ToolError::InvalidParams(msg) => RpcError::invalid_params(msg),
            ToolError::Execution(msg) => RpcError::execution(msg),
        }
    }
}

/// Public introspection descriptor of one mounted namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceSpec {
    pub namespace: String,
    pub methods: Vec<String>,
}

/// Extensible handler for one Code Mode tool namespace (e.g. `workspace`, `web`,
/// `memory`, `workstation`, or caller-mounted `coordination`).
#[async_trait]
pub trait NamespaceHandler: Send + Sync {
    /// Namespace name exposed to sandbox scripts (must be a valid JS identifier).
    fn namespace(&self) -> &str;

    /// Supported method names for schema generation and error diagnostics.
    fn methods(&self) -> Vec<String>;

    /// Invoke `method` with JSON `params`.
    async fn call(&self, method: &str, params: Value) -> Result<Value, ToolError>;
}

type BoxedHandlerFn = Box<
    dyn Fn(String, Value) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send>>
        + Send
        + Sync,
>;

/// Ergonomic closure-backed [`NamespaceHandler`] for mounting custom namespaces
/// (such as `memory`, `workstation`, or orchestrator `coordination`) without boilerplate.
pub struct FnNamespaceHandler {
    namespace: String,
    methods: Vec<String>,
    handler: BoxedHandlerFn,
}

impl std::fmt::Debug for FnNamespaceHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FnNamespaceHandler")
            .field("namespace", &self.namespace)
            .field("methods", &self.methods)
            .finish_non_exhaustive()
    }
}

impl FnNamespaceHandler {
    pub fn new<F, Fut>(namespace: impl Into<String>, methods: &[&str], handler: F) -> Self
    where
        F: Fn(String, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, ToolError>> + Send + 'static,
    {
        Self {
            namespace: namespace.into(),
            methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            handler: Box::new(move |method, params| Box::pin(handler(method, params))),
        }
    }
}

#[async_trait]
impl NamespaceHandler for FnNamespaceHandler {
    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn methods(&self) -> Vec<String> {
        self.methods.clone()
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ToolError> {
        if !self.methods.is_empty() && !self.methods.iter().any(|m| m == method) {
            return Err(ToolError::UnknownMethod {
                namespace: self.namespace.clone(),
                method: method.to_owned(),
                available: self.methods.clone(),
            });
        }
        (self.handler)(method.to_owned(), params).await
    }
}

/// Built-in `workspace.*` namespace handler delegating 100% of file and exec
/// operations strictly to an [`Arc<dyn WorkspaceBackend>`].
pub struct WorkspaceNamespaceHandler {
    backend: Arc<dyn WorkspaceBackend>,
}

impl std::fmt::Debug for WorkspaceNamespaceHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceNamespaceHandler")
            .field("kind", &self.backend.kind())
            .field("root", &self.backend.root_display())
            .finish()
    }
}

impl WorkspaceNamespaceHandler {
    pub fn new(backend: Arc<dyn WorkspaceBackend>) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> &Arc<dyn WorkspaceBackend> {
        &self.backend
    }
}

const WORKSPACE_METHODS: &[&str] = &[
    "read",
    "write",
    "edit",
    "multi_edit",
    "list",
    "glob",
    "grep",
    "exec",
];

#[async_trait]
impl NamespaceHandler for WorkspaceNamespaceHandler {
    fn namespace(&self) -> &str {
        "workspace"
    }

    fn methods(&self) -> Vec<String> {
        WORKSPACE_METHODS.iter().map(|s| (*s).to_owned()).collect()
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ToolError> {
        let backend = Arc::clone(&self.backend);
        let method_owned = method.to_owned();
        tokio::task::spawn_blocking(move || match method_owned.as_str() {
            "read" => {
                let p: ReadParams = parse_string_or_object(params, "path")?;
                ops::read_text(backend.as_ref(), &p).map_err(ToolError::Execution)
            }
            "write" => {
                let p: WriteParams = serde_json::from_value(params)
                    .map_err(|e| ToolError::InvalidParams(e.to_string()))?;
                ops::write_text(backend.as_ref(), &p).map_err(ToolError::Execution)
            }
            "edit" => {
                let p: EditParams = serde_json::from_value(params)
                    .map_err(|e| ToolError::InvalidParams(e.to_string()))?;
                ops::edit_file(backend.as_ref(), &p).map_err(ToolError::Execution)
            }
            "multi_edit" => {
                let p: MultiEditParams = serde_json::from_value(params)
                    .map_err(|e| ToolError::InvalidParams(e.to_string()))?;
                ops::multi_edit_file(backend.as_ref(), &p).map_err(ToolError::Execution)
            }
            "list" => {
                let p: ListParams = parse_optional_string_or_object(params, "path")?;
                ops::list_dir(backend.as_ref(), &p).map_err(ToolError::Execution)
            }
            "glob" => {
                let p: GlobParams = parse_string_or_object(params, "pattern")?;
                ops::glob_files(backend.as_ref(), &p).map_err(ToolError::Execution)
            }
            "grep" => {
                let p: GrepParams = parse_string_or_object(params, "query")?;
                ops::grep_files(backend.as_ref(), &p).map_err(ToolError::Execution)
            }
            "exec" => {
                let p: ExecParams = parse_string_or_object(params, "cmd")?;
                ops::exec_cmd(backend.as_ref(), &p).map_err(ToolError::Execution)
            }
            other => Err(ToolError::UnknownMethod {
                namespace: "workspace".into(),
                method: other.to_owned(),
                available: WORKSPACE_METHODS.iter().map(|s| (*s).to_owned()).collect(),
            }),
        })
        .await
        .map_err(|e| ToolError::Execution(format!("workspace task panicked: {e}")))?
    }
}

/// Built-in `web.*` namespace handler wrapping any [`WebBackend`].
pub struct WebNamespaceHandler {
    backend: Arc<dyn WebBackend>,
}

impl std::fmt::Debug for WebNamespaceHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebNamespaceHandler")
            .finish_non_exhaustive()
    }
}

impl WebNamespaceHandler {
    pub fn new(backend: Arc<dyn WebBackend>) -> Self {
        Self { backend }
    }

    pub fn default_ssrf_safe() -> Self {
        Self {
            backend: Arc::new(DefaultWebBackend),
        }
    }
}

#[async_trait]
impl NamespaceHandler for WebNamespaceHandler {
    fn namespace(&self) -> &str {
        "web"
    }

    fn methods(&self) -> Vec<String> {
        vec!["fetch".into(), "search".into()]
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ToolError> {
        match method {
            "fetch" => {
                let req: WebFetchRequest = parse_string_or_object(params, "url")?;
                let res = self
                    .backend
                    .fetch(req)
                    .await
                    .map_err(ToolError::Execution)?;
                serde_json::to_value(res).map_err(|e| ToolError::Execution(e.to_string()))
            }
            "search" => {
                let req: WebSearchRequest = parse_string_or_object(params, "query")?;
                self.backend.search(req).await.map_err(ToolError::Execution)
            }
            other => Err(ToolError::UnknownMethod {
                namespace: "web".into(),
                method: other.to_owned(),
                available: self.methods(),
            }),
        }
    }
}

fn parse_string_or_object<T: serde::de::DeserializeOwned>(
    params: Value,
    shorthand_key: &str,
) -> Result<T, ToolError> {
    let normalized = match params {
        Value::String(s) => serde_json::json!({ shorthand_key: s }),
        other => other,
    };
    serde_json::from_value(normalized).map_err(|e| ToolError::InvalidParams(e.to_string()))
}

fn parse_optional_string_or_object<T: serde::de::DeserializeOwned>(
    params: Value,
    shorthand_key: &str,
) -> Result<T, ToolError> {
    let normalized = match params {
        Value::Null => serde_json::json!({}),
        Value::String(s) => serde_json::json!({ shorthand_key: s }),
        other => other,
    };
    serde_json::from_value(normalized).map_err(|e| ToolError::InvalidParams(e.to_string()))
}

/// Registry of mounted tool namespaces exposed to sandboxed Code Mode.
#[derive(Default)]
pub struct ToolRegistry {
    handlers: BTreeMap<String, Arc<dyn NamespaceHandler>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("namespaces", &self.handlers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount any [`NamespaceHandler`] (replaces an existing handler for the same namespace).
    pub fn mount(&mut self, handler: Arc<dyn NamespaceHandler>) -> &mut Self {
        self.handlers
            .insert(handler.namespace().to_owned(), handler);
        self
    }

    /// Builder-style mount helper.
    pub fn with_namespace(mut self, handler: Arc<dyn NamespaceHandler>) -> Self {
        self.mount(handler);
        self
    }

    /// Convenience helper to mount the standard `workspace` namespace.
    pub fn with_workspace(self, backend: Arc<dyn WorkspaceBackend>) -> Self {
        self.with_namespace(Arc::new(WorkspaceNamespaceHandler::new(backend)))
    }

    /// Convenience helper to mount the SSRF-safe `web` namespace.
    pub fn with_default_web(self) -> Self {
        self.with_namespace(Arc::new(WebNamespaceHandler::default_ssrf_safe()))
    }

    /// Look up a mounted namespace handler.
    pub fn get(&self, namespace: &str) -> Option<Arc<dyn NamespaceHandler>> {
        self.handlers.get(namespace).cloned()
    }

    /// Sorted list of mounted namespace specifications for SDK bootstrap and diagnostics.
    pub fn specs(&self) -> Vec<NamespaceSpec> {
        self.handlers
            .values()
            .map(|h| NamespaceSpec {
                namespace: h.namespace().to_owned(),
                methods: h.methods(),
            })
            .collect()
    }

    /// Build a shared [`ToolDispatcher`] over this registry.
    pub fn into_dispatcher(self) -> ToolDispatcher {
        ToolDispatcher {
            registry: Arc::new(self),
        }
    }
}

/// Concurrent fail-closed tool dispatcher supporting single calls and ordered
/// `Promise.all` batch execution.
#[derive(Clone, Debug)]
pub struct ToolDispatcher {
    registry: Arc<ToolRegistry>,
}

impl ToolDispatcher {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn specs(&self) -> Vec<NamespaceSpec> {
        self.registry.specs()
    }

    /// Dispatch a single tool call, failing closed on any unknown namespace or method.
    pub async fn dispatch_one(&self, req: ToolCallRequest) -> ToolCallResponse {
        let Some(handler) = self.registry.get(&req.namespace) else {
            let mounted: Vec<String> = self
                .registry
                .specs()
                .into_iter()
                .map(|s| s.namespace)
                .collect();
            return ToolCallResponse::err(
                req.id,
                RpcError::method_not_found(format!(
                    "unknown namespace `{}`; mounted namespaces: [{}]",
                    req.namespace,
                    mounted.join(", ")
                )),
            );
        };
        match handler.call(&req.method, req.params).await {
            Ok(val) => ToolCallResponse::ok(req.id, val),
            Err(err) => ToolCallResponse::err(req.id, err.into()),
        }
    }

    /// Dispatch a batch of tool calls **concurrently** (`Promise.all` semantics),
    /// returning responses in exact deterministic input order while isolating
    /// per-call errors and enforcing `max_calls`.
    pub async fn dispatch_batch(
        &self,
        calls: Vec<ToolCallRequest>,
        max_calls: usize,
        counter: Option<&Arc<AtomicUsize>>,
    ) -> Vec<ToolCallResponse> {
        let futures = calls.into_iter().map(|req| {
            let this = self.clone();
            let counter_clone = counter.cloned();
            async move {
                if let Some(c) = counter_clone {
                    let count = c.fetch_add(1, Ordering::SeqCst) + 1;
                    if count > max_calls {
                        return ToolCallResponse::err(
                            req.id,
                            RpcError::budget_exceeded(format!(
                                "exceeded maximum of {max_calls} tool calls"
                            )),
                        );
                    }
                }
                this.dispatch_one(req).await
            }
        });
        join_all(futures).await
    }
}
