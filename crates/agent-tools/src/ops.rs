//! High-level workspace file, search, edit, and command primitives built
//! strictly on top of [`WorkspaceBackend`].
//!
//! Zero direct host filesystem (`std::fs`) access occurs here: every operation
//! delegates to the active [`WorkspaceBackend`] so remote, Docker, and virtual
//! workspaces remain strictly isolated from the coordinator host.

use globset::GlobBuilder;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;

use crate::{encoding::sha256_hex, workspace::WorkspaceBackend};

/// Default maximum bytes returned by one [`read_text`] page.
pub const DEFAULT_MAX_READ_BYTES: usize = 46_080;
/// Default maximum results returned by [`list_dir`], [`glob_files`], or [`grep_files`].
pub const DEFAULT_MAX_RESULTS: usize = 200;

const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".next",
    "dist",
    ".venv",
    "__pycache__",
];

/// Parameters for [`read_text`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReadParams {
    pub path: String,
    #[serde(default)]
    pub byte_offset: usize,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

/// Parameters for [`write_text`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WriteParams {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub append: bool,
    #[serde(default = "default_true")]
    pub overwrite: bool,
}

/// Parameters for a single exact-string edit ([`edit_file`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EditParams {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// One replacement item inside [`MultiEditParams`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EditItem {
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// Parameters for applying multiple sequential exact edits atomically ([`multi_edit_file`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MultiEditParams {
    pub path: String,
    pub edits: Vec<EditItem>,
}

/// Parameters for [`list_dir`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub include_hidden: bool,
    #[serde(default)]
    pub max_results: Option<usize>,
}

/// Parameters for [`glob_files`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GlobParams {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub include_hidden: bool,
    #[serde(default)]
    pub max_results: Option<usize>,
}

/// Parameters for [`grep_files`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GrepParams {
    pub query: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default)]
    pub include_hidden: bool,
    #[serde(default)]
    pub max_results: Option<usize>,
}

/// Parameters for [`exec_cmd`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExecParams {
    pub cmd: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

fn default_true() -> bool {
    true
}

/// Read a bounded UTF-8 page from a file via [`WorkspaceBackend`], reporting
/// exact byte range, total file size, truncation state, and SHA-256 of the whole file.
pub fn read_text(backend: &dyn WorkspaceBackend, params: &ReadParams) -> Result<Value, String> {
    let (resolved, bytes) = backend.read_file_bytes(Some(&params.path))?;
    let total_bytes = bytes.len();
    let sha = sha256_hex(&bytes);
    let max_bytes = params.max_bytes.unwrap_or(DEFAULT_MAX_READ_BYTES).max(1);
    let start = params.byte_offset.min(total_bytes);
    let end = start.saturating_add(max_bytes).min(total_bytes);
    let slice = &bytes[start..end];
    let content = String::from_utf8_lossy(slice).into_owned();
    let truncated = end < total_bytes;
    Ok(json!({
        "action": "read",
        "path": resolved,
        "content": content,
        "byte_offset": start,
        "bytes_read": slice.len(),
        "total_bytes": total_bytes,
        "truncated": truncated,
        "sha256": sha,
    }))
}

/// Write or append UTF-8 text via [`WorkspaceBackend`].
pub fn write_text(backend: &dyn WorkspaceBackend, params: &WriteParams) -> Result<Value, String> {
    let (resolved, total_bytes, sha) = backend.write_file_bytes(
        Some(&params.path),
        params.content.as_bytes(),
        params.append,
        params.overwrite,
    )?;
    Ok(json!({
        "action": "write",
        "path": resolved,
        "bytes_written": params.content.len(),
        "total_bytes": total_bytes,
        "sha256": sha,
    }))
}

/// Apply a single exact string edit via [`WorkspaceBackend`].
pub fn edit_file(backend: &dyn WorkspaceBackend, params: &EditParams) -> Result<Value, String> {
    if params.old_text.is_empty() {
        return Err("`old_text` must not be empty".into());
    }
    let (resolved, raw) = backend.read_file_bytes(Some(&params.path))?;
    let text = String::from_utf8(raw)
        .map_err(|_| format!("{resolved} is not valid UTF-8 and cannot be text-edited"))?;
    let matches = text.matches(&params.old_text).count();
    if matches == 0 {
        return Err(format!("`old_text` not found in {resolved}"));
    }
    if matches > 1 && !params.replace_all {
        return Err(format!(
            "`old_text` matched {matches} times in {resolved}; pass `replace_all: true` or add surrounding context"
        ));
    }
    let updated = if params.replace_all {
        text.replace(&params.old_text, &params.new_text)
    } else {
        text.replacen(&params.old_text, &params.new_text, 1)
    };
    let (_, total_bytes, sha) =
        backend.write_file_bytes(Some(&params.path), updated.as_bytes(), false, true)?;
    Ok(json!({
        "action": "edit",
        "path": resolved,
        "replacements": if params.replace_all { matches } else { 1 },
        "total_bytes": total_bytes,
        "sha256": sha,
    }))
}

/// Apply multiple sequential exact edits atomically to one file via [`WorkspaceBackend`].
pub fn multi_edit_file(
    backend: &dyn WorkspaceBackend,
    params: &MultiEditParams,
) -> Result<Value, String> {
    if params.edits.is_empty() {
        return Err("`edits` must contain at least one edit item".into());
    }
    let (resolved, raw) = backend.read_file_bytes(Some(&params.path))?;
    let mut text = String::from_utf8(raw)
        .map_err(|_| format!("{resolved} is not valid UTF-8 and cannot be text-edited"))?;
    let mut total_replacements = 0usize;
    for (idx, item) in params.edits.iter().enumerate() {
        if item.old_text.is_empty() {
            return Err(format!("edits[{idx}].old_text must not be empty"));
        }
        let matches = text.matches(&item.old_text).count();
        if matches == 0 {
            return Err(format!("edits[{idx}].old_text not found in {resolved}"));
        }
        if matches > 1 && !item.replace_all {
            return Err(format!(
                "edits[{idx}].old_text matched {matches} times in {resolved}; pass `replace_all: true` or add context"
            ));
        }
        if item.replace_all {
            text = text.replace(&item.old_text, &item.new_text);
            total_replacements += matches;
        } else {
            text = text.replacen(&item.old_text, &item.new_text, 1);
            total_replacements += 1;
        }
    }
    let (_, total_bytes, sha) =
        backend.write_file_bytes(Some(&params.path), text.as_bytes(), false, true)?;
    Ok(json!({
        "action": "multi_edit",
        "path": resolved,
        "applied_edits": params.edits.len(),
        "replacements": total_replacements,
        "total_bytes": total_bytes,
        "sha256": sha,
    }))
}

/// List a directory via [`WorkspaceBackend`].
pub fn list_dir(backend: &dyn WorkspaceBackend, params: &ListParams) -> Result<Value, String> {
    backend.list_dir(
        params.path.as_deref(),
        params.include_hidden,
        params
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, 1000),
    )
}

fn walk_workspace_files(
    backend: &dyn WorkspaceBackend,
    start_path: Option<&str>,
    include_hidden: bool,
    max_files: usize,
) -> Result<(String, Vec<String>), String> {
    let root_resolved = backend.resolve_path(start_path, false)?;
    let root_prefix = format!("{}/", root_resolved.trim_end_matches('/'));
    let mut queue = VecDeque::new();
    queue.push_back(root_resolved.clone());
    let mut files = Vec::new();
    while let Some(dir) = queue.pop_front() {
        if files.len() >= max_files {
            break;
        }
        let listing = backend.list_dir(Some(&dir), include_hidden, 1000)?;
        let Some(entries) = listing.get("entries").and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
            let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("file");
            if name.is_empty() || (!include_hidden && name.starts_with('.')) {
                continue;
            }
            if kind == "dir" && IGNORED_DIRS.contains(&name) {
                continue;
            }
            let child_abs = format!("{}/{name}", dir.trim_end_matches('/'));
            if kind == "dir" {
                queue.push_back(child_abs);
            } else {
                let rel = child_abs
                    .strip_prefix(&root_prefix)
                    .unwrap_or(&child_abs)
                    .to_owned();
                files.push(rel);
                if files.len() >= max_files {
                    break;
                }
            }
        }
    }
    files.sort();
    Ok((root_resolved, files))
}

/// Match workspace files against a glob pattern strictly through [`WorkspaceBackend`].
pub fn glob_files(backend: &dyn WorkspaceBackend, params: &GlobParams) -> Result<Value, String> {
    let pattern = params.pattern.trim();
    if pattern.is_empty() {
        return Err("`pattern` must not be empty".into());
    }
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|e| format!("invalid glob pattern `{pattern}`: {e}"))?
        .compile_matcher();
    let max_results = params
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, 1000);
    let (root, all_files) = walk_workspace_files(
        backend,
        params.path.as_deref(),
        params.include_hidden,
        10_000,
    )?;
    let mut matched = Vec::new();
    let mut truncated = false;
    for rel in all_files {
        if matcher.is_match(&rel) {
            if matched.len() >= max_results {
                truncated = true;
                break;
            }
            matched.push(rel);
        }
    }
    let count = matched.len();
    Ok(json!({
        "action": "glob",
        "path": root,
        "pattern": pattern,
        "matches": matched,
        "count": count,
        "truncated": truncated,
    }))
}

/// Search workspace text files strictly through [`WorkspaceBackend`].
pub fn grep_files(backend: &dyn WorkspaceBackend, params: &GrepParams) -> Result<Value, String> {
    if params.query.is_empty() {
        return Err("`query` must not be empty".into());
    }
    let max_results = params
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, 1000);
    let compiled_regex = if params.regex {
        Some(
            RegexBuilder::new(&params.query)
                .case_insensitive(params.case_insensitive)
                .build()
                .map_err(|e| format!("invalid regex `{}`: {e}", params.query))?,
        )
    } else {
        None
    };
    let needle_lower = params.query.to_ascii_lowercase();
    let (root, all_files) = walk_workspace_files(
        backend,
        params.path.as_deref(),
        params.include_hidden,
        5_000,
    )?;
    let mut matches = Vec::new();
    let mut truncated = false;
    for rel in all_files {
        if matches.len() >= max_results {
            truncated = true;
            break;
        }
        let abs = format!("{}/{rel}", root.trim_end_matches('/'));
        let Ok((_, bytes)) = backend.read_file_bytes(Some(&abs)) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        for (idx, line) in text.lines().enumerate() {
            let hit = match &compiled_regex {
                Some(re) => re.is_match(line),
                None if params.case_insensitive => {
                    line.to_ascii_lowercase().contains(&needle_lower)
                }
                None => line.contains(&params.query),
            };
            if hit {
                matches.push(json!({
                    "path": rel,
                    "line": idx + 1,
                    "text": line,
                }));
                if matches.len() >= max_results {
                    truncated = true;
                    break;
                }
            }
        }
    }
    let count = matches.len();
    Ok(json!({
        "action": "grep",
        "path": root,
        "query": params.query,
        "matches": matches,
        "count": count,
        "truncated": truncated,
    }))
}

/// Execute a shell command strictly through [`WorkspaceBackend`].
pub fn exec_cmd(backend: &dyn WorkspaceBackend, params: &ExecParams) -> Result<Value, String> {
    let out = backend.exec(&params.cmd, params.cwd.as_deref(), params.timeout_ms)?;
    Ok(json!({
        "action": "exec",
        "stdout": out.stdout,
        "stderr": out.stderr,
        "exit_code": out.exit_code,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::LocalWorkspace;

    #[test]
    fn ops_roundtrip_read_write_edit_glob_grep_exec() {
        let dir = tempfile::tempdir().unwrap();
        let ws = LocalWorkspace::new(dir.path());

        write_text(
            &ws,
            &WriteParams {
                path: "src/app.rs".into(),
                content: "fn alpha() {}\nfn beta() {}\n".into(),
                append: false,
                overwrite: true,
            },
        )
        .unwrap();

        let edited = edit_file(
            &ws,
            &EditParams {
                path: "src/app.rs".into(),
                old_text: "beta".into(),
                new_text: "gamma".into(),
                replace_all: false,
            },
        )
        .unwrap();
        assert_eq!(edited["replacements"], 1);

        let multi = multi_edit_file(
            &ws,
            &MultiEditParams {
                path: "src/app.rs".into(),
                edits: vec![
                    EditItem {
                        old_text: "alpha".into(),
                        new_text: "delta".into(),
                        replace_all: false,
                    },
                    EditItem {
                        old_text: "gamma".into(),
                        new_text: "epsilon".into(),
                        replace_all: false,
                    },
                ],
            },
        )
        .unwrap();
        assert_eq!(multi["replacements"], 2);

        let read = read_text(
            &ws,
            &ReadParams {
                path: "src/app.rs".into(),
                byte_offset: 0,
                max_bytes: Some(12),
            },
        )
        .unwrap();
        assert_eq!(read["truncated"], true);
        assert_eq!(read["bytes_read"], 12);

        let globbed = glob_files(
            &ws,
            &GlobParams {
                pattern: "**/*.rs".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(globbed["matches"], json!(["src/app.rs"]));

        let grepped = grep_files(
            &ws,
            &GrepParams {
                query: "epsilon".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(grepped["count"], 1);
        assert_eq!(grepped["matches"][0]["line"], 2);

        let ran = exec_cmd(
            &ws,
            &ExecParams {
                cmd: "printf hi".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ran["stdout"], "hi");
        assert_eq!(ran["exit_code"], 0);
    }
}
