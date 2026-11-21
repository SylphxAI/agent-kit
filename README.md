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

### 3. `agent-ingress`

Default-open ingress blocklist and structured sender awareness, with no model or network calls:
- **`SenderIdentity`** carries channel, sender, chat, and host-resolved relationship. `format_awareness_header()` puts the relationship in a single structured line without user-editable display fields.
- **`evaluate_ingress`** checks exact channel/sender or channel/chat matches, ignoring expired rules. Owner, admin, parent, and peer always pass, even when a stored rule matches. `validate_block_target` rejects those relationships at write time too.
- **`IngressBlockStore`** is an async storage port with an in-memory reference implementation. Call `is_blocked` before queueing the event or waking the agent. A storage error is not an `Allow`.
- **`IngressGuardToolRequest` / `IngressGuardToolResponse`** define the JSON wire contract for `block_sender`, `unblock_sender`, and `list_blocks`. For example:

  ```json
  {"op":"block_sender","target":{"channel":"example","scope":{"kind":"chat","chat_id":"chat-1"}}}
  ```

Authenticate the inbound event and derive the relationship independently of the message body. Scope the store to the receiving agent and authorized channel accounts. For writes, resolve the exact target in that scope and pass its current relationship and host-generated audit fields via `BlockWriteContext`; do not infer authority from a tool argument or the awareness header. For allowed messages, put the header in host-controlled context separate from the body. A `LeaveChat` hint for a group is advisory: independently authorize the effect and ensure it cannot cut off protected senders.

### 4. `agent-channels`

Channel primitives with no network, storage, or model calls. Only `serde` and `serde_json` are dependencies:

- **`telegram`**: `TelegramInboundMeta::from_message` turns a Bot API `Message` into `chat_id`, `chat_type`, `thread_id`, `message_id`, `sender_id`, `sender_is_bot`, `addressed` and `context_only`.
  - `forum_thread_id` returns a positive `message_thread_id` only when `is_topic_message` is true. A reply chain in a plain supergroup, or in a forum's General topic, gets no thread.
  - `is_addressed_to_bot` is true for private chats, `@mention` / `text_mention`, `/cmd` or `/cmd@bot`, and replies to the bot.
  - An unaddressed group message is `context_only`.
- **`reply_target`**: `ReplyTarget` can only be built through validating constructors, so a Telegram target never has a zero, negative or non-numeric thread or a compound chat id.
  - `resolve_reply_target(metadata, session_key)` accepts `replyThreadId` and `messageThreadId` (and their snake_case forms), as strings or integers. A placeholder in one field never hides a valid id in the next.
  - It decomposes compound targets (`-1001:topic:54007`, `telegram:chat:-1001:54007`) and falls back to a `:topic:<id>` / `:thread:<id>` session key for the same chat.
  - `telegram_send_message_payload` and `apply_telegram_message_thread_id` attach `message_thread_id` only when it is positive.
- **`speech_gate`**: `decide_terminal_text_delivery` decides whether an agent's terminal text may be posted to a channel. It fails closed: `OutboundSpeechMode::ToolOnly`, a context-only turn, a message already sent through a tool, or a silence sentinel each suppress delivery.
  - `is_suppressed_terminal_sentinel` covers empty text, plus `NO_REPLY` or `completed` with any case, punctuation or Markdown. Text whose first line is `NO_REPLY` is also suppressed.

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
