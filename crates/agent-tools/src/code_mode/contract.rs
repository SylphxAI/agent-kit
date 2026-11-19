//! Execution bounds, UTF-8-safe output truncation, and actionable snippet diagnostics.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::code_mode::registry::NamespaceSpec;

/// Default wall time of one sandboxed Code Mode execution (30 seconds).
pub const DEFAULT_CODE_TIMEOUT_MS: u64 = 30_000;
/// Default ceiling for per-call `timeout_ms` (10 minutes).
pub const DEFAULT_CODE_MAX_TIMEOUT_MS: u64 = 600_000;
/// Default maximum serialized result size returned inline (64 KiB).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Default maximum tool calls permitted within a single Code Mode run.
pub const DEFAULT_MAX_TOOL_CALLS: usize = 128;

const MIN_TIMEOUT_MS: u64 = 100;
const HARD_MAX_TIMEOUT_MS: u64 = 86_400_000;

/// Environment variable overriding default Code Mode timeout in milliseconds.
pub const DEFAULT_TIMEOUT_ENV: &str = "AGENT_CODE_TIMEOUT_MS";
/// Environment variable overriding maximum Code Mode timeout in milliseconds.
pub const MAX_TIMEOUT_ENV: &str = "AGENT_CODE_MAX_TIMEOUT_MS";

/// Wall-time, call-count, and output-size bounds enforced by the Code Mode runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeLimits {
    pub default_ms: u64,
    pub max_ms: u64,
    pub max_output_bytes: usize,
    pub max_tool_calls: usize,
}

impl Default for CodeLimits {
    fn default() -> Self {
        Self {
            default_ms: DEFAULT_CODE_TIMEOUT_MS,
            max_ms: DEFAULT_CODE_MAX_TIMEOUT_MS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
        }
    }
}

impl CodeLimits {
    /// Load limits from `AGENT_CODE_TIMEOUT_MS` and `AGENT_CODE_MAX_TIMEOUT_MS`.
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let read = |name: &str| lookup(name).and_then(|raw| raw.trim().parse::<u64>().ok());
        Self::from_parts(read(DEFAULT_TIMEOUT_ENV), read(MAX_TIMEOUT_ENV))
    }

    pub fn from_parts(default_ms: Option<u64>, max_ms: Option<u64>) -> Self {
        let max_ms = max_ms
            .unwrap_or(DEFAULT_CODE_MAX_TIMEOUT_MS)
            .clamp(1_000, HARD_MAX_TIMEOUT_MS);
        let default_ms = default_ms
            .unwrap_or(DEFAULT_CODE_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, max_ms);
        Self {
            default_ms,
            max_ms,
            ..Self::default()
        }
    }

    /// Resolve a per-call timeout strictly within `[MIN_TIMEOUT_MS, self.max_ms]`.
    pub fn resolve_timeout_ms(&self, requested: Option<u64>) -> u64 {
        requested
            .unwrap_or(self.default_ms)
            .clamp(MIN_TIMEOUT_MS, self.max_ms)
    }

    /// Resolve a per-call output byte limit capped at `self.max_output_bytes`.
    pub fn resolve_output_bytes(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.max_output_bytes)
            .clamp(128, self.max_output_bytes)
    }

    /// Resolve a per-call tool call cap capped at `self.max_tool_calls`.
    pub fn resolve_tool_calls(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.max_tool_calls)
            .clamp(1, self.max_tool_calls)
    }
}

/// Result of enforcing [`truncate_json_output`] on a serialized tool or code result.
#[derive(Clone, Debug, PartialEq)]
pub struct TruncatedOutput {
    pub value: Value,
    pub truncated: bool,
    pub original_bytes: usize,
}

/// Bound any JSON value so its UTF-8 serialized representation never exceeds `max_bytes`.
/// When oversized, returns a compact structured preview object with `truncated: true`,
/// `original_bytes`, and a UTF-8-boundary-safe `preview` string.
pub fn truncate_json_output(value: Value, max_bytes: usize) -> TruncatedOutput {
    let serialized = serde_json::to_string(&value).unwrap_or_default();
    let original_bytes = serialized.len();
    if original_bytes <= max_bytes {
        return TruncatedOutput {
            value,
            truncated: false,
            original_bytes,
        };
    }
    let wrapper_budget = max_bytes.saturating_sub(128).max(32);
    let mut end = serialized.len().min(wrapper_budget);
    while end > 0 && !serialized.is_char_boundary(end) {
        end -= 1;
    }
    let preview = &serialized[..end];
    let bounded = json!({
        "truncated": true,
        "original_bytes": original_bytes,
        "max_bytes": max_bytes,
        "preview": preview,
    });
    TruncatedOutput {
        value: bounded,
        truncated: true,
        original_bytes,
    }
}

/// Format an actionable timeout diagnostic naming the active ceiling.
pub fn code_timeout_message(timeout_ms: u64, requested: Option<u64>, max_ms: u64) -> String {
    let clamped = match requested {
        Some(asked) if asked > max_ms => format!(" (asked {asked}ms; ceiling is {max_ms}ms)"),
        _ => String::new(),
    };
    format!(
        "code execution timed out after {timeout_ms}ms{clamped} and was terminated. \
         Pass `timeout_ms` (up to {max_ms}) or split long-running work across calls."
    )
}

/// Diagnose common compile or runtime errors in sandboxed JS/TS snippets and
/// suggest the exact fix based on the currently mounted namespaces.
pub fn diagnose(
    code: &str,
    error: &str,
    compile: bool,
    mounted: &[NamespaceSpec],
) -> Option<String> {
    let ns_list = mounted
        .iter()
        .map(|s| s.namespace.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    if compile {
        if looks_double_escaped(code) {
            return Some(
                "the `code` string holds literal `\\n` sequences without real line breaks; send the snippet with real newlines.".into(),
            );
        }
        if looks_python(code) {
            return Some(python_fix());
        }
        if has_module_syntax(code) {
            return Some(format!(
                "`import`/`export` cannot be used: the snippet runs inside an async function body where mounted namespaces ({ns_list}) and `tools` are already in scope."
            ));
        }
        if error.contains("await is only valid") || error.contains("Unexpected reserved word") {
            return Some(
                "`await` inside a callback requires an `async` callback, e.g. `await Promise.all(xs.map(async (x) => await f(x)))`.".into(),
            );
        }
    }

    if let Some(name) = undefined_global(error) {
        if code.contains(&format!("${{{name}")) {
            return Some(format!(
                "`${{{name}...}}` inside a JS template literal is interpolated by JavaScript; escape it as `\\${{{name}...}}` to pass it to the shell."
            ));
        }
        return Some(match name {
            "require" | "module" | "exports" | "__dirname" | "__filename" => {
                "`require` and host Node modules are disabled in the isolate. Use `workspace.read`/`write`/`list` for files and `workspace.exec` for shell commands.".into()
            }
            "process" => {
                "`process` is disabled in the isolate. Use `workspace.exec` for sandbox shell commands and `return` a value to finish.".into()
            }
            "fetch" | "XMLHttpRequest" | "WebSocket" => {
                "direct network globals are disabled in the isolate; use `web.fetch({ url })` or `web.search({ query })`.".into()
            }
            "print" | "len" | "True" | "False" | "None" | "self" => python_fix(),
            other => format!(
                "`{other}` is not a sandbox global. Mounted namespaces: {ns_list} (also available under `tools.<namespace>`)."
            ),
        });
    }

    if let Some(path) = missing_function(error, mounted) {
        let mut parts: Vec<&str> = path.split('.').collect();
        let _missing = parts.pop();
        let namespace = parts.last().copied().unwrap_or_default();
        if let Some(spec) = mounted.iter().find(|s| s.namespace == namespace) {
            return Some(format!(
                "`{path}` does not exist. Mounted `{}` methods: {}.",
                spec.namespace,
                spec.methods.join(", ")
            ));
        }
    }

    None
}

fn python_fix() -> String {
    "this runner executes TypeScript/JavaScript, not Python. Rewrite as TS/JS or run Python via `await workspace.exec({ cmd: \"python3 - <<'EOF'\\n...\\nEOF\" })`.".into()
}

fn looks_double_escaped(code: &str) -> bool {
    if code.trim().contains('\n') {
        return false;
    }
    code.split("\\n").skip(1).any(|rest| {
        let rest = rest.trim_start_matches([' ', '\t']);
        [
            "const ", "let ", "var ", "return", "await ", "if ", "if(", "for ", "for(", "}", "//",
        ]
        .iter()
        .any(|start| rest.starts_with(start))
    })
}

fn looks_python(code: &str) -> bool {
    code.lines().map(str::trim).any(|line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        let block_colon = line.ends_with(':') && !line.contains('{') && !line.contains("//");
        match words.as_slice() {
            ["from", module, "import", ..] => !module.starts_with(['\'', '"']),
            ["import", module] => {
                module
                    .trim_end_matches(';')
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
                    && !line.ends_with(';')
            }
            ["import", _, "as", _] => !line.contains(['\'', '"']),
            ["def", ..]
            | ["class", ..]
            | ["elif", ..]
            | ["for", ..]
            | ["while", ..]
            | ["if", ..]
            | ["with", ..]
            | ["try:"]
            | ["else:"]
            | ["except", ..]
            | ["except:"]
            | ["finally:"] => block_colon,
            _ => false,
        }
    })
}

fn has_module_syntax(code: &str) -> bool {
    code.contains("import(")
        || code.lines().map(str::trim_start).any(|line| {
            line.starts_with("export ")
                || (line.starts_with("import ")
                    && (line.contains(" from ") || line.contains(['\'', '"'])))
        })
}

fn undefined_global(error: &str) -> Option<&str> {
    let (head, _) = error.split_once(" is not defined")?;
    let name = head
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .next()?;
    (!name.is_empty()).then_some(name)
}

fn missing_function(error: &str, mounted: &[NamespaceSpec]) -> Option<String> {
    let (head, _) = error.split_once(" is not a function")?;
    let path = head
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '$'))
        .next()?;
    let root = path.split('.').next()?;
    (root == "tools" || mounted.iter().any(|s| s.namespace == root)).then(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_clamp_timeouts_and_output_safely() {
        let limits = CodeLimits::from_parts(Some(45_000), Some(900_000));
        assert_eq!(limits.resolve_timeout_ms(None), 45_000);
        assert_eq!(limits.resolve_timeout_ms(Some(50)), 100);
        assert_eq!(limits.resolve_timeout_ms(Some(9_999_999)), 900_000);

        let big = json!({ "text": "🚀".repeat(500) });
        let out = truncate_json_output(big, 256);
        assert!(out.truncated);
        assert!(out.original_bytes > 256);
        let serialized = serde_json::to_string(&out.value).unwrap();
        assert!(serialized.len() <= 300);
        assert_eq!(out.value["truncated"], true);
    }

    #[test]
    fn diagnose_identifies_python_module_syntax_and_unknown_methods() {
        let mounted = vec![NamespaceSpec {
            namespace: "workspace".into(),
            methods: vec!["read".into(), "write".into(), "exec".into()],
        }];
        let py = diagnose("def foo():\n  pass", "SyntaxError", true, &mounted).unwrap();
        assert!(py.contains("not Python"));
        let imp = diagnose("import fs from 'fs';", "SyntaxError", true, &mounted).unwrap();
        assert!(imp.contains("already in scope"));
        let missing = diagnose(
            "await workspace.bash('ls')",
            "TypeError: workspace.bash is not a function",
            false,
            &mounted,
        )
        .unwrap();
        assert!(missing.contains("read, write, exec"));
    }
}
