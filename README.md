# agent-kit

MIT-licensed Rust libraries for building agent hosts. Parse inbound messages, decide which ones can wake an agent, recall exact bytes from its history, and expose only the tools the host mounts. Use the crates independently or together; none requires a particular agent runtime.

| Crate | What it does |
| --- | --- |
| [`agent-channels`](crates/agent-channels/src/lib.rs) | Parse Telegram Bot API messages, resolve reply targets, and decide whether terminal text may be delivered. |
| [`agent-ingress`](crates/agent-ingress/src/lib.rs) | Format host-resolved sender awareness and apply an exact sender or chat blocklist before queueing or wake. |
| [`agent-memory`](crates/agent-memory/src/lib.rs) | Append raw history and retrieve it with lexical and dense search, returning SHA-256-verifiable citations. |
| [`agent-tools`](crates/agent-tools/src/lib.rs) | Use workspace and web adapters, discover Skills, and dispatch sandbox-side Code Mode tool calls. |

## Get started

Use Rust 1.94 or newer. From a checkout, run the four-crate [inbound-to-reply test](tests/e2e/tests/inbound_to_reply.rs):

```sh
cargo test -p agent-kit-e2e --locked
```

The test uses a synthetic Telegram message, an in-memory blocklist, temporary SQLite and workspace files, and fixed vectors instead of an embedding model. The test itself makes no network calls, runs no JavaScript, and sends no messages. Cargo may fetch dependencies on the first build.

1. Parse the inbound message and preserve its forum topic in the reply target.
2. Check the host-resolved sender before writing the event or waking the agent. Keep sender awareness separate from the message body.
3. Append the exact event bytes to a raw segment. Search with both BM25 and a dense vector, then read back the cited bytes using their SHA-256 digest.
4. Discover a local `SKILL.md` and read it through the mounted `workspace` namespace. Apply the speech gate before building the topic-scoped reply payload.

The same test also checks that a blocked sender cannot wake the agent, malformed and unmounted namespaces are rejected, changed raw bytes fail citation verification, and text is suppressed after a tool has already sent the reply.

## Terminology

- **Instructions** are host-provided directives that govern agent behavior.
- **Skills** are reusable procedures packaged in `SKILL.md` files; discovery does not execute them or give them authority.
- **Memory** is conversation and event history plus learned notes. Raw event bytes remain authoritative; indexes are retrieval aids.
- **Knowledge** is curated reference material. It is distinct from Memory, and this workspace does not provide a Knowledge service.

## Integration boundaries

### Channels and ingress

`TelegramInboundMeta::from_message` takes a Bot API `Message` object. It recognizes private messages, bot mentions, commands addressed to the bot, and replies to the bot. An unaddressed group message is context-only. A reply chain alone does not create a forum topic: `message_thread_id` is carried only for topic messages with a positive ID. `resolve_reply_target` normalizes thread metadata and can recover a topic from a session key for the same chat. The speech gate suppresses terminal text in tool-only or context-only turns, after a tool has already sent a reply, or when the text is a silence sentinel such as `NO_REPLY`.

The host authenticates the inbound event, resolves sender relationships independently of the message body, and scopes the block store to the receiving agent and authorized channel account. For unprotected senders, no active rule after a successful lookup means allow; a storage error does not. Owner, admin, parent, and peer relationships are protected from blocking. Resolve block targets and authorization before writing rules; a suggested channel action is advisory, not permission to act.

### Memory backends

- **`sqlite` (default):** Local append-only raw segments, SQLite FTS5 BM25, and an int8-quantized dense index. No database server required.
- **`postgres`:** PostgreSQL `tsvector` and `pgvector` search, with reciprocal rank fusion and verified raw-byte readback.
- **`service`:** Optional retrieval service with local ONNX inference or remote providers, background dense indexing, cross-encoder reranking, and a [retrieval evaluation CLI](crates/agent-memory/examples/memory-eval.rs). Local inference requires model and runtime files; remote providers require their own configuration.

### Workspace, Skills, and Code Mode

`WorkspaceBackend` routes file and command operations through a local, Docker, remote sandbox, or caller-provided adapter. The crate also provides web utilities with an SSRF guard and `SKILL.md` discovery. Code Mode offers NDJSON stdio and loopback HTTP JSON-RPC dispatch. The host chooses which namespaces to mount via `ToolRegistry`; unmounted namespaces and unknown methods fail closed. Configure tools and isolation for the trust level of the input, and run untrusted JavaScript in an OS/container sandbox rather than treating a JavaScript VM as the security boundary.

Remote workspace `exec` uses a caller-provided positive `timeout_ms` without a library ceiling; if omitted, the command budget is 30 seconds. The remote endpoint must honor that budget and may enforce its own policy. This synchronous endpoint need not send heartbeats: the client bounds connection and send phases and waits up to the command budget plus five seconds for the HTTP exchange. A client-side timeout is not confirmation that the command stopped remotely.

The stdio runner (`execute_js_stdio` / `agent-code-mode run`) returns console records in order. The compact JSON `logs` array has its own `max_output_bytes` budget, separate from `result`. When the full array would exceed the budget, `logs` contains only whole leading records and `log_artifact` names a private UTF-8 JSON Lines file on the runner's filesystem. The file contains **all** captured records, including the inline ones, with one JSON string per line and explicit `entries` and `bytes` counts. Read it on the runner and remove it when no longer needed; callers requiring long-term retention should move it out of the temporary directory. `logs_incomplete: true` means capture was interrupted (for example, by a timeout or spool write failure); available records are still returned. The `truncated` flag describes `result`, not console logs.

## Verify the workspace

```sh
python3 scripts/check-oss-purity.py
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo test --doc --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
```

The local check enforces the repository’s endpoint and repository allowlists and requires tracked files to be UTF-8 text. Stage any changes you want it to check locally, and use reserved example domains in fixtures. CI also checks changed files for internal references and destructive SQL migrations.

For live PostgreSQL tests, set `AGENT_MEMORY_TEST_POSTGRES_URL` to the connection URL of a disposable database with `pgvector`, then run:

```sh
cargo test -p agent-memory --features postgres --locked live_postgres_pgvector_end_to_end_when_configured
cargo test -p agent-memory --features postgres --locked --test immutable_append postgres_immutable_append -- --ignored --exact
```

## License

[MIT](LICENSE)
