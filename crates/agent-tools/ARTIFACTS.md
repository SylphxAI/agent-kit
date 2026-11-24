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

The adapter sends the bearer credential only to the configured API. It uses
HTTPS (except literal loopback HTTP) and does not follow redirects.
Requests and responses with bodies use JSON. Let `A` be
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

### Preview origin policies

The service issues private, expiring signed preview URLs. The client checks
links returned by the API but never fetches them. To configure the policy:

- **Exact** matches a single HTTPS origin (scheme, host and port). Use
  `HttpArtifactBackend::new(base_url, token, Some("https://example.test"))`,
  or pass `PreviewOriginPolicy::Exact(origin)` to the policy-aware constructor.
- **SingleSubdomainApex** matches exactly one label below a DNS apex. For
  example, use:

  ```rust,ignore
  use agent_tools::artifact::{HttpArtifactBackend, PreviewOriginPolicy};

  let backend = HttpArtifactBackend::new_with_preview_policy(
      "https://offline.invalid",
      token,
      Some(PreviewOriginPolicy::SingleSubdomainApex {
          apex_host: "sandbox.example.test".into(),
          port: None, // Require HTTPS on port 443.
      }),
  )?;
  ```

  The allowed host has the form `<artifact-label>.sandbox.example.test`. The
  label must be 1–63 lowercase ASCII letters, digits or hyphens, starting and
  ending with a letter or digit. Punycode labels, the bare apex, deeper
  subdomains and lookalike suffixes are not allowed. Set `port: Some(n)` to
  require a different HTTPS port. The apex must be a lowercase ASCII DNS name
  without a trailing dot and must not be the API host.

Both policies reject userinfo and preview links on the API host, even on a
separate port. Pass `None` to either constructor to reject all preview links.
The caller must configure a dedicated preview site, and the service must
assign a separate origin to each artifact when using per-artifact subdomains.
The client does not verify that mapping, signatures, expiry, revocation or
cookie scoping. Treat preview URLs as credentials: do not log them or publish
them to an unauthorized room.
