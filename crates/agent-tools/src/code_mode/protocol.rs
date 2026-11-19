//! Typed JSON-RPC 2.0 and NDJSON wire protocol for sandbox programmatic code mode.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request correlation identifier (`number` or `string`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(u64),
    String(String),
}

impl From<u64> for RpcId {
    fn from(n: u64) -> Self {
        Self::Number(n)
    }
}

impl From<String> for RpcId {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&str> for RpcId {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

/// Standard JSON-RPC 2.0 error codes plus tool-execution error code.
pub const ERR_PARSE: i32 = -32700;
pub const ERR_INVALID_REQUEST: i32 = -32600;
pub const ERR_METHOD_NOT_FOUND: i32 = -32601;
pub const ERR_INVALID_PARAMS: i32 = -32602;
pub const ERR_INTERNAL: i32 = -32603;
pub const ERR_BUDGET_EXCEEDED: i32 = -32001;
pub const ERR_TIMEOUT: i32 = -32002;
pub const ERR_TOOL_EXECUTION: i32 = -32000;

/// Structured JSON-RPC error payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn method_not_found(message: impl Into<String>) -> Self {
        Self {
            code: ERR_METHOD_NOT_FOUND,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: ERR_INVALID_PARAMS,
            message: message.into(),
            data: None,
        }
    }

    pub fn execution(message: impl Into<String>) -> Self {
        Self {
            code: ERR_TOOL_EXECUTION,
            message: message.into(),
            data: None,
        }
    }

    pub fn budget_exceeded(message: impl Into<String>) -> Self {
        Self {
            code: ERR_BUDGET_EXCEEDED,
            message: message.into(),
            data: None,
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            code: ERR_TIMEOUT,
            message: message.into(),
            data: None,
        }
    }
}

/// Single namespaced tool call request (`<namespace>.<method>`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: RpcId,
    pub namespace: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl ToolCallRequest {
    pub fn new(
        id: impl Into<RpcId>,
        namespace: impl Into<String>,
        method: impl Into<String>,
        params: Value,
    ) -> Self {
        Self {
            id: id.into(),
            namespace: namespace.into(),
            method: method.into(),
            params,
        }
    }

    /// Parse a `"namespace.method"` compound method string (e.g., `"workspace.read"`).
    pub fn from_compound(
        id: impl Into<RpcId>,
        compound: &str,
        params: Value,
    ) -> Result<Self, RpcError> {
        let trimmed = compound.trim();
        let Some((ns, method)) = trimmed.split_once('.') else {
            return Err(RpcError::method_not_found(format!(
                "tool method `{trimmed}` must be formatted as `<namespace>.<method>`"
            )));
        };
        if ns.is_empty() || method.is_empty() {
            return Err(RpcError::method_not_found(format!(
                "tool method `{trimmed}` must have non-empty namespace and method"
            )));
        }
        Ok(Self::new(id, ns, method, params))
    }
}

/// Deterministic response for one [`ToolCallRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallResponse {
    pub id: RpcId,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl ToolCallResponse {
    pub fn ok(id: RpcId, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: RpcId, error: RpcError) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

/// Standard JSON-RPC 2.0 request object (`method = "<namespace>.<method>"`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default = "jsonrpc_v2")]
    pub jsonrpc: String,
    pub id: RpcId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

fn jsonrpc_v2() -> String {
    "2.0".to_owned()
}

/// Standard JSON-RPC 2.0 response object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl From<ToolCallResponse> for JsonRpcResponse {
    fn from(resp: ToolCallResponse) -> Self {
        Self {
            jsonrpc: jsonrpc_v2(),
            id: resp.id,
            result: resp.result,
            error: resp.error,
        }
    }
}

/// Single or batched JSON-RPC 2.0 HTTP/stdio payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcPayload {
    Single(JsonRpcRequest),
    Batch(Vec<JsonRpcRequest>),
}

/// Single or batched JSON-RPC 2.0 response payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResponsePayload {
    Single(JsonRpcResponse),
    Batch(Vec<JsonRpcResponse>),
}

/// Request to execute a sandboxed JS/TS snippet in Code Mode.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodeExecRequest {
    pub code: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    #[serde(default)]
    pub max_tool_calls: Option<usize>,
}

/// Structured outcome of executing a Code Mode snippet or batch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CodeExecResult {
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub logs: Vec<String>,
    pub duration_ms: u64,
    pub truncated: bool,
    pub original_bytes: usize,
    pub tool_calls: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compound_method_parsing_and_jsonrpc_batch_roundtrip() {
        let req = ToolCallRequest::from_compound(1u64, "workspace.read", json!({"path": "a.txt"}))
            .unwrap();
        assert_eq!(req.namespace, "workspace");
        assert_eq!(req.method, "read");

        assert!(ToolCallRequest::from_compound(2u64, "invalid", json!({})).is_err());
        assert!(ToolCallRequest::from_compound(3u64, ".read", json!({})).is_err());

        let batch = JsonRpcPayload::Batch(vec![
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: 1u64.into(),
                method: "workspace.read".into(),
                params: json!({"path": "a"}),
            },
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: "b".into(),
                method: "web.fetch".into(),
                params: json!({"url": "https://example.test"}),
            },
        ]);
        let encoded = serde_json::to_string(&batch).unwrap();
        let decoded: JsonRpcPayload = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, batch);
    }
}
