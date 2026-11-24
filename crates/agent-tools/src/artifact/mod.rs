//! Versioned artifacts with explicit, host-selected storage and publication policy.
//!
//! The host authorizes workspace and room access before exposing a backend. Room IDs
//! are placement metadata, not access grants. Content, titles, diffs and previews are
//! untrusted data: this crate neither renders nor executes them.

mod http;
mod local;
mod tool;

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

pub use http::HttpArtifactBackend;
pub use local::LocalArtifactBackend;
pub use tool::{ArtifactTool, artifact_tool_schema};

/// Maximum decoded bytes per version, including on the HTTP adapter.
pub const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
/// Maximum metadata records per list page.
pub const MAX_ARTIFACT_PAGE: usize = 4;
const MAX_ID: usize = 128;
const MAX_TITLE: usize = 256;
const MAX_MIME: usize = 127;
const MAX_LANGUAGE: usize = 64;
const MAX_PREVIEW: usize = 2048;
const MAX_DIFF: usize = 2048;
const MAX_BASE64: usize = MAX_ARTIFACT_BYTES.div_ceil(3) * 4;
const MAX_VERSION: u64 = (1 << 53) - 1; // Exact in JSON/JavaScript and SQLite.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Markdown,
    Code,
    Html,
    Svg,
    Mermaid,
    Chart,
    Table,
    Diff,
    Image,
    File,
}

impl ArtifactKind {
    const ALL: [Self; 10] = [
        Self::Markdown,
        Self::Code,
        Self::Html,
        Self::Svg,
        Self::Mermaid,
        Self::Chart,
        Self::Table,
        Self::Diff,
        Self::Image,
        Self::File,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Code => "code",
            Self::Html => "html",
            Self::Svg => "svg",
            Self::Mermaid => "mermaid",
            Self::Chart => "chart",
            Self::Table => "table",
            Self::Diff => "diff",
            Self::Image => "image",
            Self::File => "file",
        }
    }

    fn mime_types(self) -> &'static [&'static str] {
        match self {
            Self::Markdown => &["text/markdown"],
            Self::Code => &["text/plain"],
            Self::Html => &["text/html"],
            Self::Svg => &["image/svg+xml"],
            Self::Mermaid => &["text/vnd.mermaid", "text/plain"],
            Self::Chart => &["application/json"],
            Self::Table => &["text/csv", "text/tab-separated-values", "application/json"],
            Self::Diff => &["text/x-diff", "text/plain"],
            Self::Image => &[
                "image/png",
                "image/jpeg",
                "image/gif",
                "image/webp",
                "image/avif",
            ],
            Self::File => &[], // Any valid media type; treat file artifacts as downloads.
        }
    }

    fn is_text(self) -> bool {
        !matches!(self, Self::Image | Self::File)
    }
}

/// Exactly one content representation. Base64 uses the padded standard alphabet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactContent {
    Text(String),
    Base64(String),
}

impl ArtifactContent {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self::Base64(crate::base64_encode(bytes))
    }

    pub fn decode(&self) -> Result<Vec<u8>, ArtifactError> {
        match self {
            Self::Text(text) => {
                check_size(text.len())?;
                Ok(text.as_bytes().to_vec())
            }
            Self::Base64(encoded) => {
                if encoded.len() > MAX_BASE64 {
                    return Err(ArtifactError::SizeLimit);
                }
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|_| invalid("invalid padded base64"))?;
                check_size(bytes.len())?;
                Ok(bytes)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_id: Option<String>,
    pub title: String,
    pub kind: ArtifactKind,
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub version: u64,
    pub sha256: String,
    pub byte_size: usize,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactVersion {
    pub metadata: ArtifactMetadata,
    pub content: ArtifactContent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateArtifact {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_id: Option<String>,
    pub title: String,
    pub kind: ArtifactKind,
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub content: ArtifactContent,
}

/// Replace content (and optionally the title) iff this is still the head version.
/// Placement, kind, media type and language are immutable across revisions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateArtifact {
    pub expected_version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub content: ArtifactContent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDiff {
    pub added_lines: usize,
    pub removed_lines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_diff: Option<String>,
    /// Text changed but the full unified hunk exceeded the summary limit.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactUpdate {
    pub metadata: ArtifactMetadata,
    /// Absent for binary content; compare the metadata digests instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<ArtifactDiff>,
}

/// A lexicographically ordered page of current revisions, not a snapshot of the catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactList {
    pub room_id: Option<String>,
    pub kind: Option<ArtifactKind>,
    pub after: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPage {
    pub artifacts: Vec<ArtifactMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("invalid artifact: {0}")]
    InvalidInput(&'static str),
    #[error("artifact exceeds the byte limit")]
    SizeLimit,
    #[error("artifact or version not found")]
    NotFound,
    #[error("artifact version conflict")]
    VersionConflict,
    #[error("artifact storage unavailable or inconsistent")]
    Storage,
    #[error("artifact service returned HTTP {0}")]
    HttpStatus(u16),
    #[error("artifact transport failed; a submitted write may have committed")]
    Transport,
    #[error("invalid artifact service response; a submitted write may have committed")]
    InvalidResponse,
}

/// Synchronous storage contract. Implementations serialize CAS against all writers
/// and preserve historical revisions until deletion. A tool invokes it off the async executor.
pub trait ArtifactBackend: Send + Sync {
    fn create(&self, request: CreateArtifact) -> Result<ArtifactMetadata, ArtifactError>;
    fn update(&self, id: &str, request: UpdateArtifact) -> Result<ArtifactUpdate, ArtifactError>;
    fn get(&self, id: &str, version: Option<u64>) -> Result<ArtifactVersion, ArtifactError>;
    fn list(&self, filter: ArtifactList) -> Result<ArtifactPage, ArtifactError>;
    fn delete(&self, id: &str) -> Result<(), ArtifactError>;
}

fn invalid(reason: &'static str) -> ArtifactError {
    ArtifactError::InvalidInput(reason)
}

fn check_id(id: &str) -> Result<(), ArtifactError> {
    // Deliberately excludes every path and URL separator, escape and dot segment.
    if id.len() > MAX_ID
        || !id.starts_with(|c: char| c.is_ascii_alphanumeric())
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(invalid("invalid identifier"));
    }
    Ok(())
}

fn check_version(version: u64) -> Result<(), ArtifactError> {
    if !(1..=MAX_VERSION).contains(&version) {
        return Err(invalid("version out of range"));
    }
    Ok(())
}

fn check_title(title: &str) -> Result<(), ArtifactError> {
    if title.trim().is_empty() || title.len() > MAX_TITLE || title.chars().any(char::is_control) {
        return Err(invalid("invalid title"));
    }
    Ok(())
}

fn check_shape(
    kind: ArtifactKind,
    mime: &str,
    language: Option<&str>,
) -> Result<(), ArtifactError> {
    fn token(value: &str) -> bool {
        !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"!#$&^_.+-".contains(&b))
    }
    if mime.len() > MAX_MIME
        || !mime
            .split_once('/')
            .is_some_and(|(a, b)| token(a) && token(b))
        || (kind != ArtifactKind::File && !kind.mime_types().contains(&mime))
    {
        return Err(invalid("media type does not match kind"));
    }
    match language {
        Some(value)
            if kind == ArtifactKind::Code
                && !value.is_empty()
                && value.len() <= MAX_LANGUAGE
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_+-#.".contains(&b)) =>
        {
            Ok(())
        }
        None => Ok(()),
        Some(_) => Err(invalid("invalid language for kind")),
    }
}

fn check_size(size: usize) -> Result<(), ArtifactError> {
    if size > MAX_ARTIFACT_BYTES {
        Err(ArtifactError::SizeLimit)
    } else {
        Ok(())
    }
}

fn check_bytes(kind: ArtifactKind, mime: &str, bytes: &[u8]) -> Result<(), ArtifactError> {
    check_size(bytes.len())?;
    if kind.is_text() {
        std::str::from_utf8(bytes).map_err(|_| invalid("kind requires UTF-8 content"))?;
        if mime == "application/json" {
            serde_json::from_slice::<serde::de::IgnoredAny>(bytes)
                .map_err(|_| invalid("invalid JSON content"))?;
        }
    }
    Ok(())
}

impl CreateArtifact {
    fn validated_bytes(&self) -> Result<Vec<u8>, ArtifactError> {
        for id in self.workspace_id.iter().chain(self.room_id.iter()) {
            check_id(id)?;
        }
        check_title(&self.title)?;
        check_shape(self.kind, &self.mime_type, self.language.as_deref())?;
        let bytes = self.content.decode()?;
        check_bytes(self.kind, &self.mime_type, &bytes)?;
        Ok(bytes)
    }
}

impl UpdateArtifact {
    fn validated_bytes(&self) -> Result<Vec<u8>, ArtifactError> {
        check_version(self.expected_version)?;
        if let Some(title) = &self.title {
            check_title(title)?;
        }
        self.content.decode()
    }
}

impl ArtifactList {
    fn page_limit(&self) -> Result<usize, ArtifactError> {
        for id in self.room_id.iter().chain(self.after.iter()) {
            check_id(id)?;
        }
        let limit = self.limit.unwrap_or(MAX_ARTIFACT_PAGE);
        if !(1..=MAX_ARTIFACT_PAGE).contains(&limit) {
            return Err(invalid("list limit out of range"));
        }
        Ok(limit)
    }
}

impl ArtifactMetadata {
    fn check(&self) -> Result<(), ArtifactError> {
        for id in std::iter::once(&self.artifact_id)
            .chain(self.workspace_id.iter())
            .chain(self.room_id.iter())
        {
            check_id(id)?;
        }
        check_title(&self.title)?;
        check_shape(self.kind, &self.mime_type, self.language.as_deref())?;
        check_version(self.version)?;
        check_size(self.byte_size)?;
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.created_at_ms > self.updated_at_ms
            || self.preview_url.as_ref().is_some_and(|url| {
                url.len() > MAX_PREVIEW
                    || !url.is_ascii()
                    || url
                        .bytes()
                        .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            })
        {
            return Err(invalid("invalid metadata"));
        }
        Ok(())
    }

    fn check_content(&self, bytes: &[u8]) -> Result<(), ArtifactError> {
        check_bytes(self.kind, &self.mime_type, bytes)?;
        if bytes.len() != self.byte_size || crate::sha256_hex(bytes) != self.sha256 {
            return Err(invalid("content does not match metadata"));
        }
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn content_for(kind: ArtifactKind, bytes: Vec<u8>) -> Result<ArtifactContent, ArtifactError> {
    if kind.is_text() {
        Ok(ArtifactContent::Text(
            String::from_utf8(bytes).map_err(|_| ArtifactError::Storage)?,
        ))
    } else {
        Ok(ArtifactContent::from_bytes(&bytes))
    }
}

/// A linear-time unified replacement hunk after removing equal prefix/suffix lines.
/// No expensive edit-script search or output that grows beyond MAX_DIFF.
fn summarize_diff(old: &str, new: &str) -> ArtifactDiff {
    use std::fmt::Write as _;

    let (prefix, prefix_lines) = old
        .split_inclusive('\n')
        .zip(new.split_inclusive('\n'))
        .take_while(|(a, b)| a == b)
        .fold((0, 0), |(bytes, lines), (a, _)| {
            (bytes + a.len(), lines + 1)
        });
    let (old, new) = (&old[prefix..], &new[prefix..]);
    // Strip the shared suffix without allocating an index for every line.
    fn reverse_lines(s: &str) -> impl Iterator<Item = &str> {
        let mut end = s.len();
        std::iter::from_fn(move || {
            if end == 0 {
                return None;
            }
            let search_end = end - usize::from(s.as_bytes()[end - 1] == b'\n');
            let start = s[..search_end].rfind('\n').map_or(0, |i| i + 1);
            let line = &s[start..end];
            end = start;
            Some(line)
        })
    }
    let suffix = reverse_lines(old)
        .zip(reverse_lines(new))
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len())
        .sum::<usize>();
    let (old, new) = (&old[..old.len() - suffix], &new[..new.len() - suffix]);
    let mut summary = ArtifactDiff {
        added_lines: new.split_inclusive('\n').count(),
        removed_lines: old.split_inclusive('\n').count(),
        unified_diff: None,
        truncated: false,
    };
    if old.is_empty() && new.is_empty() {
        return summary;
    }
    let mut patch = String::from("--- previous\n+++ current\n");
    let _ = writeln!(
        patch,
        "@@ -{},{} +{},{} @@",
        prefix_lines + usize::from(!old.is_empty()),
        summary.removed_lines,
        prefix_lines + usize::from(!new.is_empty()),
        summary.added_lines
    );
    for (marker, line) in old
        .split_inclusive('\n')
        .map(|s| ('-', s))
        .chain(new.split_inclusive('\n').map(|s| ('+', s)))
    {
        let end = if line.ends_with('\n') {
            ""
        } else {
            "\n\\ No newline at end of file\n"
        };
        if patch.len() + 1 + line.len() + end.len() > MAX_DIFF {
            summary.truncated = true;
            return summary;
        }
        patch.push(marker);
        patch.push_str(line);
        patch.push_str(end);
    }
    summary.unified_diff = Some(patch);
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_kind_and_media_pairs_validate() {
        for (kind, mime, text) in [
            (ArtifactKind::Markdown, "text/markdown", "# Notes"),
            (ArtifactKind::Code, "text/plain", "fn main() {}"),
            (ArtifactKind::Html, "text/html", "<p>Notes</p>"),
            (ArtifactKind::Svg, "image/svg+xml", "<svg/>"),
            (ArtifactKind::Mermaid, "text/vnd.mermaid", "flowchart LR"),
            (
                ArtifactKind::Chart,
                "application/json",
                "{\"values\":[1,2]}",
            ),
            (ArtifactKind::Table, "text/csv", "name,value"),
            (ArtifactKind::Diff, "text/x-diff", "-old\n+new\n"),
            (ArtifactKind::Image, "image/png", ""),
            (ArtifactKind::File, "application/octet-stream", ""),
        ] {
            let req = CreateArtifact {
                workspace_id: None,
                room_id: None,
                title: "Notes".into(),
                kind,
                mime_type: mime.into(),
                language: (kind == ArtifactKind::Code).then(|| "rust".into()),
                content: if kind.is_text() {
                    ArtifactContent::Text(text.into())
                } else {
                    ArtifactContent::from_bytes(&[137, 80, 78, 71, 0])
                },
            };
            assert!(req.validated_bytes().is_ok(), "{kind:?}/{mime}");
        }
        assert!(check_shape(ArtifactKind::Image, "image/svg+xml", None).is_err());
        assert!(
            check_shape(
                ArtifactKind::File,
                "application/octet-stream; charset=utf-8",
                None
            )
            .is_err()
        );
        assert!(check_bytes(ArtifactKind::Chart, "application/json", b"{").is_err());
        assert!(check_bytes(ArtifactKind::Code, "text/plain", &[0xff]).is_err());
        assert!(ArtifactContent::Base64("YQ".into()).decode().is_err());
    }

    #[test]
    fn unified_summary_handles_empty_and_noop_changes() {
        let removed = summarize_diff("one\n", "");
        assert_eq!(
            removed.unified_diff.as_deref(),
            Some("--- previous\n+++ current\n@@ -1,1 +0,0 @@\n-one\n")
        );
        let added = summarize_diff("", "one\n");
        assert_eq!(
            added.unified_diff.as_deref(),
            Some("--- previous\n+++ current\n@@ -0,0 +1,1 @@\n+one\n")
        );
        assert_eq!(
            summarize_diff("one\n", "one\n"),
            ArtifactDiff {
                added_lines: 0,
                removed_lines: 0,
                unified_diff: None,
                truncated: false,
            }
        );
    }
}
