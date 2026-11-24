# Artifacts

Mount the tool with `ToolRegistry::new().with_artifact(backend)` in Code Mode,
or register one tool named `artifact` with `artifact_tool_schema()` and call
`ArtifactTool::call` with its JSON arguments. Give the tool an
`Arc<dyn ArtifactBackend>`. For example, wrapping
`LocalArtifactBackend::new(workspace_root)?` gives a persistent, zero-service
backend. The host supplies the workspace root and authorizes access before
mounting: a room ID groups artifacts but is not an authorization credential.

Create an artifact with:

    {"action":"create","title":"Garden notes","kind":"markdown","mime_type":"text/markdown","room_id":"room_a","content":{"text":"# Spring"}}

The result is metadata with a stable ID and version 1. Updates require the
current `expected_version`, replace content and optionally the title, and
produce a new revision. A conflicting update fails with `VersionConflict`
without writing. Workspace, room, kind, media type and language stay fixed.
Text updates report a unified replacement hunk and line counts; if the hunk
would exceed 2 KiB only counts and `truncated: true` are returned. Binary
updates return digests rather than a line diff.

Storage limits each revision to 1 MiB of decoded bytes. Use
`{"text":"..."}` for text or `{"base64":"..."}` for padded standard base64.
Image media types are restricted to common raster formats; use the SVG kind
for SVG and treat all file kinds as download-only. MIME validation is not
content sanitization. Never run code or render active content in a privileged
origin. The crate does not render, grant room access or send notifications.

Tool `get` defaults to the latest version and returns a content page. To
continue, pass `next_byte_offset` as `byte_offset` together with the returned
`metadata.version` as `version`. The Rust backend's `get` returns the
whole version. List pages contain current metadata sorted by ID; pass
`next_after` as `after` with the same filters to continue. Deleting an
artifact makes all versions inaccessible via the backend. The local store
retains unreferenced content-addressed blobs, so deletion is **not** secure
erasure. Keep its private directory out of untrusted filesystem mounts and
back it up as a unit.

## HTTP backend contract

A host can replace local storage with `HttpArtifactBackend::new(base_url,
bearer_token, preview_origin)?`. The adapter sends `Authorization: Bearer …` to
that base only, uses HTTPS (except literal HTTP loopback), and does not follow
redirects. Requests and responses with bodies use JSON. Let `A` be
`<base>/v1/artifacts`, including any configured base path prefix.

| Method and path | Request | Success |
| --- | --- | --- |
| POST A | `title`, `kind`, `mime_type`, `content`; optional `workspace_id`, `room_id`, `language` | 201, metadata |
| PATCH A/{id} | `expected_version`, `content`; optional `title` | 200, `{metadata, diff?}` |
| GET A/{id} | optional `?version=N` (absent means head) | 200, `{metadata, content}` |
| GET A | optional `room_id`, `kind`, `after` query params; client sends `limit` (1–4) | 200, `{artifacts: [metadata, …], next_after?}` |
| DELETE A/{id} | no body | 204, no body |

The server must enforce access controls and atomic version comparison; a
stale PATCH returns 409 and a missing resource or version returns 404. The
client does not retry mutations. Reconcile uncertain writes before repeating
them, especially creates. The list cursor is the last returned ID when there
are more matches, is exclusive, and is not a snapshot token.

Metadata fields: `artifact_id`, `title`, `kind`, `mime_type`, `version`,
`sha256` (lowercase hex over decoded content), `byte_size`, `created_at_ms`,
`updated_at_ms` (Unix milliseconds), and optional `workspace_id`,
`room_id`, `language` and `preview_url`. Text PATCH receipts include a
`diff` with `added_lines`, `removed_lines`, `truncated` and optional
`unified_diff`. For binary kinds omit `diff`.

The service issues private, expiring signed preview URLs. The client accepts
links only from the explicitly configured HTTPS preview origin, on a different
host from the API, and never fetches them. Link signing, expiry, revocation,
renderer isolation and cookie scoping are service/host responsibilities. Treat
preview URLs as credentials; do not log them or publish them to an unauthorized
room.
