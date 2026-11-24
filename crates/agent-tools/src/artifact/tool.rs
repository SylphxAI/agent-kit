use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::code_mode::{NamespaceHandler, ToolError};

use super::*;

pub(crate) const NAME: &str = "artifact";
const METHODS: [&str; 5] = ["create", "update", "get", "list", "delete"];
const PAGE_BYTES: usize = 4096;
const MAX_TOOL_RESULT: usize = 30 * 1024;

/// Input JSON Schema for the single tool named `artifact`.
/// The same fields, without `action`, are used by the registry namespace methods.
/// String bounds in JSON Schema are character counts; storage also checks byte limits.
pub fn artifact_tool_schema() -> Value {
    let id = json!({"type":"string", "minLength":1, "maxLength":MAX_ID, "pattern":"^[A-Za-z0-9][A-Za-z0-9_-]*$", "not":{"pattern":"[^A-Za-z0-9_-]"}});
    let kind = json!({"enum": ArtifactKind::ALL});
    let version = json!({"type":"integer", "minimum":1, "maximum":MAX_VERSION});
    let title = json!({"type":"string", "minLength":1, "maxLength":MAX_TITLE});
    let media = json!({"type":"string", "maxLength":MAX_MIME, "pattern":"^[a-z0-9!#$&^_.+\\-]+/[a-z0-9!#$&^_.+\\-]+$"});
    let content = json!({"oneOf":[
        {"type":"object", "properties":{"text":{"type":"string","maxLength":MAX_ARTIFACT_BYTES}},"required":["text"],"additionalProperties":false},
        {"type":"object", "properties":{"base64":{"type":"string","maxLength":MAX_BASE64,"contentEncoding":"base64"}},"required":["base64"],"additionalProperties":false}
    ]});
    let branch = |action: &str, mut properties: Value, required: &[&str]| {
        properties["action"] = json!({"const":action});
        let required: Vec<_> = std::iter::once("action")
            .chain(required.iter().copied())
            .collect();
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    };
    let mut create = branch(
        "create",
        json!({
            "workspace_id":id, "room_id":id, "title":title, "kind":kind, "mime_type":media,
            "language":{"type":"string","minLength":1,"maxLength":MAX_LANGUAGE,"pattern":"^[A-Za-z0-9_+#.\\-]+$"}, "content":content
        }),
        &["title", "kind", "mime_type", "content"],
    );
    create["allOf"] = json!(ArtifactKind::ALL.map(|kind| {
        let mut then = json!({});
        if kind != ArtifactKind::File {
            then["properties"] = json!({"mime_type":{"enum":kind.mime_types()}});
        }
        if kind != ArtifactKind::Code {
            then["not"] = json!({"required":["language"]});
        }
        json!({"if":{"properties":{"kind":{"const":kind}}},"then":then})
    }));
    let mut get = branch(
        "get",
        json!({
            "artifact_id":id, "version":version,
            "byte_offset":{"type":"integer","minimum":0,"maximum":MAX_ARTIFACT_BYTES,"description":"Resume from next_byte_offset at the returned metadata version."},
            "metadata_only":{"type":"boolean","description":"Omit content from the result."}
        }),
        &["artifact_id"],
    );
    get["if"] = json!({"required":["byte_offset"],"properties":{"byte_offset":{"minimum":1}}});
    get["then"] = json!({"required":["version"],"properties":{"metadata_only":{"const":false}}});
    json!({
        "type":"object",
        "description":"Create, revise, read, list or delete versioned artifacts. Pass a room ID when creating room content. Updates require the current version.",
        "oneOf":[create,
            branch("update", json!({"artifact_id":id,"expected_version":version,"title":title,"content":content}), &["artifact_id", "expected_version", "content"]),
            get,
            branch("list", json!({"room_id":id,"kind":kind,"after":id,"limit":{"type":"integer","minimum":1,"maximum":MAX_ARTIFACT_PAGE}}), &[]),
            branch("delete", json!({"artifact_id":id}), &["artifact_id"])
        ]
    })
}

/// Mountable Code Mode namespace and direct single-tool entry point.
#[derive(Clone)]
pub struct ArtifactTool {
    backend: Arc<dyn ArtifactBackend>,
}

impl ArtifactTool {
    pub fn new(backend: Arc<dyn ArtifactBackend>) -> Self {
        Self { backend }
    }

    /// Invoke the single tool with arguments matching `artifact_tool_schema`.
    pub async fn call(&self, mut arguments: Value) -> Result<Value, ToolError> {
        let action = arguments
            .as_object_mut()
            .and_then(|obj| obj.remove("action"))
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| params_error("action is required"))?;
        NamespaceHandler::call(self, &action, arguments).await
    }
}

#[async_trait]
impl NamespaceHandler for ArtifactTool {
    fn namespace(&self) -> &str {
        NAME
    }

    fn methods(&self) -> Vec<String> {
        METHODS.into_iter().map(str::to_owned).collect()
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ToolError> {
        let work = match method {
            "create" => Call::Create(parse(params)?),
            "update" => {
                let mut params = params;
                let id = params
                    .as_object_mut()
                    .and_then(|obj| obj.remove("artifact_id"))
                    .ok_or_else(|| params_error("artifact_id is required"))?;
                Call::Update(parse(id)?, parse(params)?)
            }
            "get" => Call::Get(parse(params)?),
            "list" => Call::List(parse(if params.is_null() { json!({}) } else { params })?),
            "delete" => Call::Delete(parse(params)?),
            other => {
                return Err(ToolError::UnknownMethod {
                    namespace: NAME.into(),
                    method: other.into(),
                    available: self.methods(),
                });
            }
        };
        let backend = Arc::clone(&self.backend);
        tokio::task::spawn_blocking(move || work.run(backend.as_ref()))
            .await
            .map_err(|_| ToolError::Execution("artifact task failed".into()))?
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    artifact_id: String,
    version: Option<u64>,
    #[serde(default)]
    byte_offset: usize,
    #[serde(default)]
    metadata_only: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delete {
    artifact_id: String,
}

enum Call {
    Create(CreateArtifact),
    Update(String, UpdateArtifact),
    Get(Get),
    List(ArtifactList),
    Delete(Delete),
}

impl Call {
    fn run(self, backend: &dyn ArtifactBackend) -> Result<Value, ToolError> {
        match self {
            Self::Create(request) => output(backend.create(request)?),
            Self::Update(id, request) => output(backend.update(&id, request)?),
            Self::Get(request) => {
                if request.byte_offset > MAX_ARTIFACT_BYTES
                    || request.byte_offset != 0
                        && (request.version.is_none() || request.metadata_only)
                {
                    return Err(params_error(
                        "a nonzero byte_offset requires a pinned version and content",
                    ));
                }
                let artifact = backend.get(&request.artifact_id, request.version)?;
                if request.metadata_only {
                    return output(json!({"metadata":artifact.metadata}));
                }
                let bytes = artifact.content.decode()?;
                let start = request.byte_offset;
                if start > bytes.len() {
                    return Err(params_error("byte_offset beyond end of content"));
                }
                let mut end = (start + PAGE_BYTES).min(bytes.len());
                let content = if artifact.metadata.kind.is_text() {
                    let text = std::str::from_utf8(&bytes).map_err(|_| ArtifactError::Storage)?;
                    if !text.is_char_boundary(start) {
                        return Err(params_error("byte_offset must be on a UTF-8 boundary"));
                    }
                    end = text.floor_char_boundary(end);
                    ArtifactContent::Text(text[start..end].to_owned())
                } else {
                    ArtifactContent::from_bytes(&bytes[start..end])
                };
                let mut page =
                    json!({"metadata":artifact.metadata, "content":content, "byte_offset":start});
                if end < bytes.len() {
                    page["next_byte_offset"] = json!(end);
                }
                output(page)
            }
            Self::List(filter) => output(backend.list(filter)?),
            Self::Delete(request) => {
                backend.delete(&request.artifact_id)?;
                output(json!({"artifact_id":request.artifact_id,"deleted":true}))
            }
        }
    }
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, ToolError> {
    // Do not reflect arbitrary input values (including malformed content) in diagnostics.
    serde_json::from_value(value)
        .map_err(|_| params_error("arguments do not match the artifact schema"))
}

fn params_error(message: &str) -> ToolError {
    ToolError::InvalidParams(message.into())
}

fn output(value: impl Serialize) -> Result<Value, ToolError> {
    let output = serde_json::to_value(value)
        .map_err(|_| ToolError::Execution("could not encode artifact result".into()))?;
    if output.to_string().len() > MAX_TOOL_RESULT {
        return Err(ToolError::Execution(
            "artifact result exceeds tool output limit".into(),
        ));
    }
    Ok(output)
}
