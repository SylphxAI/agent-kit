//! Sandbox-side JS/TS SDK preamble, HTTP JSON-RPC batch client, and TypeScript
//! declaration generator for mounted namespaces.
//!
//! Compatible with Node.js, Bun, and Deno inside the agent sandbox.

use crate::code_mode::registry::ToolRegistry;

/// Locked-down JS/TS sandbox worker communicating over NDJSON stdio.
///
/// Protocol:
/// - Host -> Worker: `{"type":"start","code":"...","namespaces":[{"namespace":"...","methods":[...]}]}`
/// - Worker -> Host: `{"type":"batch","calls":[{"id":1,"namespace":"...","method":"...","params":{}}]}`
/// - Host -> Worker: `{"type":"batch_reply","replies":[{"id":1,"ok":true,"result":...}]}`
/// - Worker -> Host: `{"type":"done","ok":true,"result":...,"logs":[...]}`
///
/// When the runner supplies `AGENT_CODE_CONSOLE_FILE`, the worker instead writes
/// console records to that private file, and reports any write failure as `logError`.
pub const SANDBOX_WORKER_JS: &str = r#"
const Module = require("module");
const vm = require("vm");
const readline = require("readline");
const consolePath = process.env.AGENT_CODE_CONSOLE_FILE;
let writeConsoleRecord;
let logError;
if (consolePath !== undefined) {
  try {
    const fs = require("fs");
    const fd = fs.openSync(consolePath, "a");
    const toBytes = Buffer.from.bind(Buffer);
    writeConsoleRecord = (line) => {
      const bytes = toBytes(line, "utf8");
      let offset = 0;
      while (offset < bytes.length) {
        const n = fs.writeSync(fd, bytes, offset, bytes.length - offset);
        if (n <= 0) throw new Error("console write made no progress");
        offset += n;
      }
    };
  } catch {
    logError = "could not open console log spool";
  }
}

const isBun = typeof Bun !== "undefined";
const stripTS =
  isBun && Bun.Transpiler
    ? (src) => new Bun.Transpiler({ loader: "ts" }).transformSync(src)
    : typeof Module.stripTypeScriptTypes === "function"
      ? (src) => Module.stripTypeScriptTypes(src)
      : (src) => src;

const stdoutWrite = process.stdout.write.bind(process.stdout);
const exitProcess = process.exit.bind(process);
const nextTick = process.nextTick.bind(process);
const outerSetTimeout = setTimeout;
const outerQueueMicrotask = queueMicrotask;
const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

// Scrub host environment and ambient process/module/network capabilities before any user code runs.
for (const k of Object.keys(process.env)) {
  try { delete process.env[k]; } catch {}
}
try {
  Object.defineProperty(process, "env", {
    value: Object.freeze(Object.create(null)),
    writable: false,
    configurable: false,
  });
} catch {}
Module._load = () => {
  throw new Error("require/import is disabled in the code mode isolate");
};
// Bun's internal node:stream readable loop calls globalThis.process.nextTick when
// draining stdin; keep a minimal frozen stub on Bun while removing full process access.
for (const g of ["require", "Bun", "Deno", "fetch", "WebSocket"]) {
  try { delete globalThis[g]; } catch {}
}
if (isBun) {
  try {
    Object.defineProperty(globalThis, "process", {
      value: Object.freeze({ nextTick, env: Object.freeze(Object.create(null)) }),
      writable: false,
      configurable: false,
    });
  } catch {}
} else {
  try { delete globalThis.process; } catch {}
}

const pending = new Map();
let nextId = 1;
let queuedCalls = [];
let flushScheduled = false;
const logs = [];

function hostLogRaw(level, text) {
  if (logError) return;
  const prefix = level === "log" ? "" : `[${level}] `;
  const msg = prefix + String(text);
  if (!writeConsoleRecord) {
    logs.push(msg);
    return;
  }
  try {
    writeConsoleRecord(JSON.stringify(msg) + "\n");
  } catch {
    // Do not pass an outer-realm filesystem exception into the snippet.
    logError = "could not write console log spool";
  }
}

function flushBatch() {
  flushScheduled = false;
  if (queuedCalls.length === 0) return;
  const batch = queuedCalls;
  queuedCalls = [];
  stdoutWrite(JSON.stringify({ type: "batch", calls: batch }) + "\n");
}

function hostCallRaw(namespace, method, paramsJson, onOk, onErr) {
  const id = nextId++;
  let params = {};
  try { params = JSON.parse(paramsJson); } catch {}
  pending.set(id, { onOk, onErr });
  queuedCalls.push({ id, namespace: String(namespace), method: String(method), params });
  if (!flushScheduled) {
    flushScheduled = true;
    outerQueueMicrotask(flushBatch);
  }
}

function hostSleepRaw(ms, cb) {
  const delay = Math.max(0, Math.min(86400000, Number(ms) || 0));
  outerSetTimeout(() => {
    try { cb(); } catch {}
  }, delay);
}

function compileError(err) {
  const text = err && err.stack ? String(err.stack) : String(err);
  const lines = text.split("\n").filter((line) => !/^\s+at /.test(line));
  const where = lines.length ? /^(?:evalmachine\.<anonymous>)?:(\d+)$/.exec(lines[0]) : null;
  if (where) {
    const line = Number(where[1]) - 2;
    lines[0] = line >= 1 ? `snippet line ${line}:` : "";
  }
  return lines.join("\n").trim();
}

function compileUserCode(rawCode, ctx) {
  const trimmed = String(rawCode ?? "").trim();
  if (!trimmed) {
    return vm.runInContext("(async () => null)", ctx);
  }
  const shadow = "tools, console, process, require, module, exports, Bun, Deno, fetch, WebSocket";
  if (!/\breturn\b/.test(trimmed)) {
    try {
      const exprBody = trimmed.replace(/;+\s*$/, "");
      const exprTs =
        `var __fn = async (${shadow}) => {\n  "use strict";\n  return (\n` +
        exprBody +
        "\n  );\n};";
      const exprJs = stripTS(exprTs) + "\n;__fn;";
      return vm.runInContext(exprJs, ctx);
    } catch {}
  }
  const stmtTs =
    `var __fn = async (${shadow}) => {\n  "use strict"; {\n` +
    rawCode +
    "\n}};";
  const stmtJs = stripTS(stmtTs) + "\n;__fn;";
  return vm.runInContext(stmtJs, ctx);
}

function handleReply(reply) {
  const p = pending.get(reply.id);
  if (!p) return;
  pending.delete(reply.id);
  if (reply.ok) {
    p.onOk(JSON.stringify(reply.result ?? null));
  } else {
    const msg = reply.error && typeof reply.error === "object"
      ? (reply.error.message || JSON.stringify(reply.error))
      : String(reply.error || "tool call failed");
    p.onErr(msg);
  }
}

rl.on("line", async (line) => {
  if (!line.trim()) return;
  let msg;
  try { msg = JSON.parse(line); } catch { return; }

  if (msg.type === "reply") {
    handleReply(msg);
    return;
  }
  if (msg.type === "batch_reply" && Array.isArray(msg.replies)) {
    for (const r of msg.replies) handleReply(r);
    return;
  }
  if (msg.type === "start") {
    try {
      if (logError) throw new Error(logError);
      const ctx = vm.createContext(Object.create(null), {
        codeGeneration: { strings: false, wasm: false },
      });
      const bootstrap = vm.runInContext(`(function(hostCallRaw, hostLogRaw, hostSleepRaw, specsJson) {
        "use strict";
        const specs = JSON.parse(specsJson);
        const invoke = (ns, method, params) => new Promise((resolve, reject) => {
          hostCallRaw(String(ns), String(method), JSON.stringify(params ?? {}), (resJson) => {
            try { resolve(JSON.parse(resJson)); } catch (e) { reject(e); }
          }, (errStr) => {
            reject(new Error(String(errStr)));
          });
        });

        const normalizeArgs = (ns, method, a0, a1) => {
          if (ns === "workspace") {
            if (method === "read" && typeof a0 === "string") return { ...(a1 || {}), path: a0 };
            if (method === "write" && typeof a0 === "string") return { path: a0, content: a1 ?? "" };
            if (method === "exec" && typeof a0 === "string") return { ...(a1 || {}), cmd: a0 };
            if (method === "glob" && typeof a0 === "string") return { ...(a1 || {}), pattern: a0 };
            if (method === "grep" && typeof a0 === "string") return { ...(a1 || {}), query: a0 };
            if (method === "list" && typeof a0 === "string") return { ...(a1 || {}), path: a0 };
          }
          if (ns === "web") {
            if (method === "fetch" && typeof a0 === "string") return { ...(a1 || {}), url: a0 };
            if (method === "search" && typeof a0 === "string") return { ...(a1 || {}), query: a0 };
          }
          return a0 === undefined ? {} : a0;
        };

        const tools = Object.create(null);
        tools.call = (ns, method, params) => invoke(ns, method, params);
        for (const spec of specs) {
          const obj = Object.create(null);
          for (const method of spec.methods) {
            obj[method] = (a0, a1) => invoke(spec.namespace, method, normalizeArgs(spec.namespace, method, a0, a1));
          }
          Object.freeze(obj);
          tools[spec.namespace] = obj;
          globalThis[spec.namespace] = obj;
        }
        Object.freeze(tools);

        const fmt = (v) => {
          if (typeof v === "string") return v;
          try { return JSON.stringify(v); } catch { return String(v); }
        };
        const log = (lvl, args) => hostLogRaw(lvl, args.map(fmt).join(" "));
        const console = Object.freeze({
          log: (...a) => log("log", a),
          info: (...a) => log("info", a),
          warn: (...a) => log("warn", a),
          error: (...a) => log("error", a),
          debug: (...a) => log("debug", a),
        });
        const setTimeout = (cb, ms = 0) => {
          hostSleepRaw(Number(ms) || 0, () => {
            if (typeof cb === "function") cb();
          });
        };

        for (const forbidden of [
          "process", "require", "module", "exports", "Bun", "Deno",
          "fetch", "WebSocket", "XMLHttpRequest", "Buffer", "setImmediate"
        ]) {
          try {
            Object.defineProperty(globalThis, forbidden, {
              value: undefined,
              writable: false,
              configurable: false,
            });
          } catch {
            try { delete globalThis[forbidden]; } catch {}
          }
        }
        globalThis.tools = tools;
        globalThis.console = console;
        globalThis.setTimeout = setTimeout;
        return Object.freeze({ tools, console });
      })`, ctx);

      const env = bootstrap(hostCallRaw, hostLogRaw, hostSleepRaw, JSON.stringify(msg.namespaces || []));
      let fn;
      try {
        fn = compileUserCode(msg.code, ctx);
      } catch (err) {
        stdoutWrite(
          JSON.stringify({ type: "done", ok: false, phase: "compile", error: compileError(err), logs, logError }) + "\n",
          () => exitProcess(0)
        );
        return;
      }
      try {
        Object.defineProperty(Function.prototype, "constructor", { value: undefined, writable: false, configurable: false });
        Object.defineProperty((async () => {}).constructor.prototype, "constructor", { value: undefined, writable: false, configurable: false });
        vm.runInContext(
          'try { Object.defineProperty(Function.prototype, "constructor", { value: undefined, writable: false, configurable: false }); Object.defineProperty((async () => {}).constructor.prototype, "constructor", { value: undefined, writable: false, configurable: false }); } catch {}',
          ctx
        );
      } catch {}

      const result = await fn(env.tools, env.console);
      const safeResult = result === undefined ? null : result;
      let doneLine;
      try {
        doneLine = JSON.stringify({ type: "done", ok: true, result: safeResult, logs, logError }) + "\n";
      } catch {
        doneLine = JSON.stringify({ type: "done", ok: true, result: String(safeResult), logs, logError }) + "\n";
      }
      stdoutWrite(doneLine, () => exitProcess(0));
    } catch (err) {
      const errMsg = err && err.stack ? String(err.stack) : String(err);
      stdoutWrite(
        JSON.stringify({ type: "done", ok: false, error: errMsg, logs, logError }) + "\n",
        () => exitProcess(0)
      );
    }
  }
});
"#;

/// Lightweight sandbox-side JS/TS HTTP JSON-RPC 2.0 client SDK supporting
/// automatic microtask `Promise.all` array batching against `POST /rpc`.
pub const SANDBOX_HTTP_CLIENT_JS: &str = r#"
export function createSandboxToolsClient({ endpoint, token, namespaces = [] }) {
  let nextId = 1;
  let queue = [];
  let scheduled = false;

  async function flush() {
    scheduled = false;
    const batch = queue;
    queue = [];
    if (batch.length === 0) return;

    const payload =
      batch.length === 1
        ? { jsonrpc: "2.0", id: batch[0].id, method: batch[0].method, params: batch[0].params }
        : batch.map((c) => ({ jsonrpc: "2.0", id: c.id, method: c.method, params: c.params }));

    try {
      const resp = await fetch(endpoint, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          ...(token ? { authorization: `Bearer ${token}` } : {}),
        },
        body: JSON.stringify(payload),
      });
      const data = await resp.json();
      const replies = Array.isArray(data) ? data : [data];
      const byId = new Map(replies.map((r) => [r.id, r]));
      for (const item of batch) {
        const reply = byId.get(item.id);
        if (!reply) {
          item.reject(new Error(`missing JSON-RPC reply for id ${item.id}`));
        } else if (reply.error) {
          item.reject(new Error(reply.error.message || JSON.stringify(reply.error)));
        } else {
          item.resolve(reply.result);
        }
      }
    } catch (err) {
      for (const item of batch) item.reject(err);
    }
  }

  function call(namespace, method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = nextId++;
      queue.push({ id, method: `${namespace}.${method}`, params, resolve, reject });
      if (!scheduled) {
        scheduled = true;
        queueMicrotask(flush);
      }
    });
  }

  const tools = { call };
  for (const spec of namespaces) {
    const nsObj = {};
    for (const m of spec.methods || []) {
      nsObj[m] = (params = {}) => call(spec.namespace, m, params);
    }
    tools[spec.namespace] = Object.freeze(nsObj);
  }
  return Object.freeze(tools);
}
"#;

/// Generate ambient TypeScript declarations (`.d.ts`) describing every mounted
/// namespace in `registry` for agent prompts and sandbox SDK tooling.
pub fn generate_ts_declarations(registry: &ToolRegistry) -> String {
    let mut out = String::from("// Auto-generated Sandbox Code Mode API declarations.\n\n");
    let specs = registry.specs();
    for spec in &specs {
        out.push_str(&format!("declare namespace {} {{\n", spec.namespace));
        for method in &spec.methods {
            out.push_str(&format!(
                "  function {method}(params?: any, extra?: any): Promise<any>;\n"
            ));
        }
        out.push_str("}\n\n");
    }
    out.push_str("declare const tools: {\n");
    out.push_str("  call(namespace: string, method: string, params?: any): Promise<any>;\n");
    for spec in &specs {
        out.push_str(&format!(
            "  readonly {}: typeof {};\n",
            spec.namespace, spec.namespace
        ));
    }
    out.push_str("};\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_mode::registry::FnNamespaceHandler;
    use std::sync::Arc;

    #[test]
    fn generates_typescript_declarations_for_mounted_namespaces() {
        let mut reg = ToolRegistry::new().with_default_web();
        reg.mount(Arc::new(FnNamespaceHandler::new(
            "coordination",
            &["send", "inbox"],
            |_, _| async { Ok(serde_json::json!({})) },
        )));
        let dts = generate_ts_declarations(&reg);
        assert!(dts.contains("declare namespace web"));
        assert!(dts.contains("function fetch("));
        assert!(dts.contains("declare namespace coordination"));
        assert!(dts.contains("function send("));
        assert!(dts.contains("readonly coordination: typeof coordination;"));
    }
}
