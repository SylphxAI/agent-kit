# agent-kit

Pure-Rust, MIT-licensed foundational primitives for autonomous coding and research agents.

## Workspace Crates

### 1. `agent-memory`

Pure-Rust Raw-RAG memory engine built on the principle that **immutable raw history is the single source of truth** (`arXiv:2610.30755`):

- **Immutable Raw Segments (`raw`)**: SHA-256 framed v2 append-only segments with crash-safe `.ack` frontier recovery and in-place JSONL scanning.
- **Hybrid Raw-RAG Retrieval**: Hybrid ASCII + CJK BM25 (`host_index` / `tokenize`), dense embeddings (`dense`), Reciprocal Rank Fusion (`fusion::reciprocal_rank`), and cross-encoder reranking (`service` feature) returning exact raw-byte citations verified by SHA-256.
- **Pluggable `MemoryStore` Backend Trait (`store`)**:
  - `SqliteStore` (feature `sqlite`, enabled by default): local append-only raw segments + SQLite FTS5 BM25 + int8 quantized dense index.
  - `PostgresPgvectorStore` (feature `postgres`): production PostgreSQL `tsvector` + `pgvector` cosine similarity + single-query SQL CTE Reciprocal Rank Fusion and exact `substring(raw_bytes ...)` SHA-256 citation readback.
- **Thin `MEMORY.md` Helpers (`notes`)**: Bounded session snapshot pinning (`MAX_BYTES = 4096`, `MAX_LINES = 150`) and Markdown topic index management without graph rewrites.
- **Optional Retrieval Service (`service` feature)**: Pinned local ONNX Runtime CPU inference (`model`, `pins`, `runtime`), remote OpenAI-compatible embeddings and Cohere/Voyage/Jina/OpenRouter rerankers (`providers`), striped background indexing (`embed`, `service`), and offline retrieval benchmarking (`eval`, `examples/memory-eval.rs`).

### 2. `agent-tools`

Pluggable workspace backends, SSRF-safe web utilities, skill discovery, and a unified sandbox-side Programmatic Code Mode SDK/runner:

- **Pluggable `WorkspaceBackend`**: `LocalWorkspace`, `DockerWorkspace`, and `RemoteSandboxWorkspace` (HTTP / E2B APIs) plus `SandboxProvider` lifecycle management.
- **Host-Free Workspace Ops (`ops`)**: `read`, `write`, `edit`, `multi_edit`, `list`, `glob`, `grep`, and `exec` routed strictly through `WorkspaceBackend`.
- **Web & Skills (`web`, `skills`)**: Global unicast IP SSRF guard, HTML-to-Markdown extraction, `SKILL.md` discovery and non-destructive overlay mounting.
- **Unified Sandbox-Side Programmatic Code Mode (`code_mode` + `agent-code-mode` binary)**:
  - Executes strictly inside the agent sandbox or a locked-down host-free VM isolate (cleared env, scrubbed `process`/`require`/`fetch`/`Bun`/`Deno`).
  - Concurrent `Promise.all` batching over NDJSON stdio or bearer-authenticated loopback/sandbox HTTP JSON-RPC 2.0 (`POST /rpc`).
  - Extensible `NamespaceHandler` trait and `ToolRegistry` supporting standard namespaces (`workspace`, `web`, `memory`, `workstation`) plus arbitrary caller-mounted namespaces with zero orchestrator hardcoding.
  - Enforced timeouts (with Unix process-group termination), call budgets, fail-closed unknown namespace/method checks, and UTF-8-safe output truncation.

## Quick Verification

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
cargo test --workspace --all-features --locked
```

Optional live PostgreSQL + `pgvector` integration test:

```bash
AGENT_MEMORY_TEST_POSTGRES_URL="postgres://user:pass@localhost:5432/agent_memory_test" \
  cargo test -p agent-memory --features postgres live_postgres_pgvector_end_to_end_when_configured
```

## License

MIT — see [LICENSE](LICENSE).
