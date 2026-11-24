use std::{io::Read, time::Duration};

use serde::de::DeserializeOwned;
use ureq::{Agent, Body, http::Response};
use url::{Host, Origin, Url};

use super::*;

// JSON escapes can expand one input byte to six characters. Bound the actual
// decoded HTTP stream, including metadata, even without a Content-Length header.
const MAX_GET_BODY: usize = MAX_ARTIFACT_BYTES * 6 + 16 * 1024;
const MAX_METADATA_BODY: usize = 32 * 1024;

/// REST adapter for a trusted server implementing the artifact contract.
///
/// The server must authorize every operation, enforce CAS and issue short-lived
/// signed preview URLs. This client checks preview origin, never makes requests
/// to preview URLs, and does not validate the server's signing scheme. The host
/// must choose a preview site isolated from application cookies and active UI.
/// Endpoint configuration and bearer credentials must not come from tool input.
#[derive(Clone)]
pub struct HttpArtifactBackend {
    collection: Url,
    bearer: String,
    preview_origin: Option<Origin>,
    agent: Agent,
}

impl HttpArtifactBackend {
    /// HTTPS is required except for literal loopback HTTP addresses (local development).
    /// If no preview origin is configured, any response containing a preview is refused.
    pub fn new(
        base_url: &str,
        token: &str,
        preview_origin: Option<&str>,
    ) -> Result<Self, ArtifactError> {
        let mut collection = parse_url(base_url).ok_or_else(|| invalid("invalid API base URL"))?;
        let is_loopback = match collection.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !(collection.scheme() == "https" || collection.scheme() == "http" && is_loopback)
            || collection.query().is_some()
            || collection.fragment().is_some()
        {
            return Err(invalid(
                "API base requires HTTPS or loopback HTTP, without query or fragment",
            ));
        }
        let preview_origin = preview_origin
            .map(|input| {
                let preview = parse_url(input).ok_or_else(|| invalid("invalid preview origin"))?;
                if preview.scheme() != "https"
                    || preview.path() != "/"
                    || preview.query().is_some()
                    || preview.fragment().is_some()
                    || preview.host_str().map(|host| host.trim_end_matches('.'))
                        == collection.host_str().map(|host| host.trim_end_matches('.'))
                {
                    return Err(invalid("preview origin requires a separate HTTPS host"));
                }
                Ok(preview.origin())
            })
            .transpose()?;
        let credential = token.trim_end_matches('=');
        if credential.is_empty()
            || token.len() > 8192
            || !credential
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._~+/".contains(&c))
        {
            return Err(invalid("malformed authorization credential"));
        }
        collection
            .path_segments_mut()
            .map_err(|_| invalid("invalid API base path"))?
            .pop_if_empty()
            .extend(["v1", "artifacts"]);
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .into();
        Ok(Self {
            collection,
            bearer: format!("Bearer {token}"),
            preview_origin,
            agent,
        })
    }

    fn item(&self, id: &str) -> Result<Url, ArtifactError> {
        check_id(id)?;
        let mut url = self.collection.clone();
        url.path_segments_mut()
            .map_err(|_| ArtifactError::InvalidResponse)?
            .push(id);
        Ok(url)
    }

    fn check_metadata(&self, meta: &ArtifactMetadata) -> Result<(), ArtifactError> {
        meta.check().map_err(|_| ArtifactError::InvalidResponse)?;
        if let Some(link) = &meta.preview_url {
            let url = parse_url(link).ok_or(ArtifactError::InvalidResponse)?;
            if url.scheme() != "https" || self.preview_origin.as_ref() != Some(&url.origin()) {
                return Err(ArtifactError::InvalidResponse);
            }
        }
        Ok(())
    }
}

fn parse_url(input: &str) -> Option<Url> {
    // Do not accept parser repair of malformed URI/authority or browser whitespace.
    if input
        .bytes()
        .any(|b| b == b'\\' || b.is_ascii_control() || b.is_ascii_whitespace())
        || !input.starts_with("https://") && !input.starts_with("http://")
    {
        return None;
    }
    let url = Url::parse(input).ok()?;
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || input
            .split_once("://")?
            .1
            .split(['/', '?', '#'])
            .next()?
            .contains('@')
    {
        return None;
    }
    Some(url)
}

fn response(
    result: Result<Response<Body>, ureq::Error>,
    expected: u16,
    cas: bool,
) -> Result<Response<Body>, ArtifactError> {
    let response = result.map_err(|_| ArtifactError::Transport)?;
    match response.status().as_u16() {
        code if code == expected => Ok(response),
        404 => Err(ArtifactError::NotFound),
        409 if cas => Err(ArtifactError::VersionConflict),
        code => Err(ArtifactError::HttpStatus(code)),
    }
}

fn read_json<T: DeserializeOwned>(
    mut response: Response<Body>,
    limit: usize,
) -> Result<T, ArtifactError> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok());
    if !content_type
        .and_then(|s| s.split(';').next())
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(ArtifactError::InvalidResponse);
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .with_config()
        .limit(limit as u64 + 1)
        .reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ArtifactError::InvalidResponse)?;
    if bytes.len() > limit {
        return Err(ArtifactError::InvalidResponse);
    }
    serde_json::from_slice(&bytes).map_err(|_| ArtifactError::InvalidResponse)
}

impl ArtifactBackend for HttpArtifactBackend {
    fn create(&self, req: CreateArtifact) -> Result<ArtifactMetadata, ArtifactError> {
        let bytes = req.validated_bytes()?;
        let result = self
            .agent
            .post(self.collection.as_str())
            .header("Authorization", &self.bearer)
            .header("Accept", "application/json")
            .send_json(&req);
        let meta: ArtifactMetadata = read_json(response(result, 201, false)?, MAX_METADATA_BODY)?;
        self.check_metadata(&meta)?;
        if meta.version != 1
            || meta.workspace_id != req.workspace_id
            || meta.room_id != req.room_id
            || meta.kind != req.kind
            || meta.title != req.title
            || meta.mime_type != req.mime_type
            || meta.language != req.language
        {
            return Err(ArtifactError::InvalidResponse);
        }
        meta.check_content(&bytes)
            .map_err(|_| ArtifactError::InvalidResponse)?;
        Ok(meta)
    }

    fn update(&self, id: &str, req: UpdateArtifact) -> Result<ArtifactUpdate, ArtifactError> {
        let url = self.item(id)?;
        let bytes = req.validated_bytes()?;
        check_version(req.expected_version + 1)?;
        let result = self
            .agent
            .patch(url.as_str())
            .header("Authorization", &self.bearer)
            .header("Accept", "application/json")
            .send_json(&req);
        let update: ArtifactUpdate = read_json(response(result, 200, true)?, MAX_METADATA_BODY)?;
        let meta = &update.metadata;
        self.check_metadata(meta)?;
        if meta.artifact_id != id
            || meta.version != req.expected_version + 1
            || req.title.as_ref().is_some_and(|t| meta.title != *t)
            || meta.kind.is_text() != update.diff.is_some()
            || update.diff.as_ref().is_some_and(|d| {
                d.added_lines > MAX_ARTIFACT_BYTES
                    || d.removed_lines > MAX_ARTIFACT_BYTES
                    || d.unified_diff.as_ref().is_some_and(|p| p.len() > MAX_DIFF)
            })
        {
            return Err(ArtifactError::InvalidResponse);
        }
        meta.check_content(&bytes)
            .map_err(|_| ArtifactError::InvalidResponse)?;
        Ok(update)
    }

    fn get(&self, id: &str, version: Option<u64>) -> Result<ArtifactVersion, ArtifactError> {
        let mut url = self.item(id)?;
        if let Some(v) = version {
            check_version(v)?;
            url.query_pairs_mut().append_pair("version", &v.to_string());
        }
        let result = self
            .agent
            .get(url.as_str())
            .header("Authorization", &self.bearer)
            .header("Accept", "application/json")
            .call();
        let artifact: ArtifactVersion = read_json(response(result, 200, false)?, MAX_GET_BODY)?;
        self.check_metadata(&artifact.metadata)?;
        if artifact.metadata.artifact_id != id
            || version.is_some_and(|v| artifact.metadata.version != v)
        {
            return Err(ArtifactError::InvalidResponse);
        }
        let bytes = artifact
            .content
            .decode()
            .map_err(|_| ArtifactError::InvalidResponse)?;
        artifact
            .metadata
            .check_content(&bytes)
            .map_err(|_| ArtifactError::InvalidResponse)?;
        Ok(artifact)
    }

    fn list(&self, filter: ArtifactList) -> Result<ArtifactPage, ArtifactError> {
        let limit = filter.page_limit()?;
        let mut url = self.collection.clone();
        {
            let mut query = url.query_pairs_mut();
            if let Some(room) = &filter.room_id {
                query.append_pair("room_id", room);
            }
            if let Some(kind) = filter.kind {
                query.append_pair("kind", kind.as_str());
            }
            if let Some(after) = &filter.after {
                query.append_pair("after", after);
            }
            query.append_pair("limit", &limit.to_string());
        }
        let result = self
            .agent
            .get(url.as_str())
            .header("Authorization", &self.bearer)
            .header("Accept", "application/json")
            .call();
        let page: ArtifactPage = read_json(response(result, 200, false)?, MAX_METADATA_BODY)?;
        if page.artifacts.len() > limit {
            return Err(ArtifactError::InvalidResponse);
        }
        let mut after = filter.after.as_deref().unwrap_or("");
        for meta in &page.artifacts {
            self.check_metadata(meta)?;
            if meta.artifact_id.as_str() <= after
                || filter
                    .room_id
                    .as_ref()
                    .is_some_and(|id| meta.room_id.as_ref() != Some(id))
                || filter.kind.is_some_and(|kind| meta.kind != kind)
            {
                return Err(ArtifactError::InvalidResponse);
            }
            after = &meta.artifact_id;
        }
        if page
            .next_after
            .as_deref()
            .is_some_and(|next| page.artifacts.len() != limit || after != next)
        {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(page)
    }

    fn delete(&self, id: &str) -> Result<(), ArtifactError> {
        let url = self.item(id)?;
        response(
            self.agent
                .delete(url.as_str())
                .header("Authorization", &self.bearer)
                .call(),
            204,
            false,
        )?;
        Ok(())
    }
}
