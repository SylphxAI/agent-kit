use std::{io::Read, time::Duration};

use serde::de::DeserializeOwned;
use ureq::{Agent, Body, http::Response};
use url::{Host, Origin, Url};

use super::*;

// JSON escapes can expand one input byte to six characters. Bound the actual
// decoded HTTP stream, including metadata, even without a Content-Length header.
const MAX_GET_BODY: usize = MAX_ARTIFACT_BYTES * 6 + 16 * 1024;
const MAX_METADATA_BODY: usize = 32 * 1024;

/// Host-configured allowlist for signed preview links returned by the API.
///
/// Both policies require HTTPS on a host separate from the API. Neither policy
/// fetches the link or validates its signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewOriginPolicy {
    /// Match exactly one origin (scheme, host and port).
    Exact(Origin),
    /// Match exactly one lowercase DNS label below the configured apex.
    SingleSubdomainApex {
        /// A lowercase ASCII DNS name without a trailing dot.
        apex_host: String,
        /// The required HTTPS port. `None` means 443.
        port: Option<u16>,
    },
}

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
    preview_policy: Option<PreviewOriginPolicy>,
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
        let preview_policy = preview_origin
            .map(|input| {
                let preview = parse_url(input).ok_or_else(|| invalid("invalid preview origin"))?;
                if preview.path() != "/"
                    || preview.query().is_some()
                    || preview.fragment().is_some()
                {
                    return Err(invalid(
                        "preview origin must not include a path, query or fragment",
                    ));
                }
                Ok(PreviewOriginPolicy::Exact(preview.origin()))
            })
            .transpose()?;
        Self::new_with_preview_policy(base_url, token, preview_policy)
    }

    /// Select an explicit preview policy. `None` refuses all preview links.
    pub fn new_with_preview_policy(
        base_url: &str,
        token: &str,
        preview_policy: Option<PreviewOriginPolicy>,
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
        match &preview_policy {
            Some(PreviewOriginPolicy::Exact(Origin::Tuple(scheme, host, _)))
                if scheme == "https" && !same_host(&host.to_string(), &collection) => {}
            Some(PreviewOriginPolicy::SingleSubdomainApex { apex_host, .. })
                if apex_host.len() <= 251 // Leave room for a dot and at least one character.
                    && apex_host.split('.').all(is_dns_label)
                    && matches!(Host::parse(apex_host), Ok(Host::Domain(host)) if host == *apex_host)
                    && !same_host(apex_host, &collection) => {}
            None => {}
            _ => {
                return Err(invalid(
                    "preview policy requires a separate HTTPS host or DNS apex",
                ));
            }
        }
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
            preview_policy,
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
            let host = url.host_str().ok_or(ArtifactError::InvalidResponse)?;
            if url.scheme() != "https" || same_host(host, &self.collection) {
                return Err(ArtifactError::InvalidResponse);
            }
            let allowed = match &self.preview_policy {
                Some(PreviewOriginPolicy::Exact(origin)) => *origin == url.origin(),
                Some(PreviewOriginPolicy::SingleSubdomainApex { apex_host, port }) => {
                    // Validate the raw host spelling too: URL parsing normalizes case,
                    // percent escapes and IDNA before exposing host_str().
                    authority(link).is_some_and(|raw| {
                        !raw.ends_with(':') && raw.split(':').next() == Some(host)
                    }) && host.len() <= 253
                        && url.port_or_known_default() == Some(port.unwrap_or(443))
                        && host
                            .strip_suffix(apex_host.as_str())
                            .and_then(|prefix| prefix.strip_suffix('.'))
                            .is_some_and(is_dns_label)
                }
                None => false,
            };
            if !allowed {
                return Err(ArtifactError::InvalidResponse);
            }
        }
        Ok(())
    }
}

fn is_dns_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && !label.starts_with('-')
        && !label.ends_with('-')
        && !label.starts_with("xn--")
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn same_host(host: &str, other: &Url) -> bool {
    other.host_str().is_some_and(|other_host| {
        host.trim_end_matches('.')
            .eq_ignore_ascii_case(other_host.trim_end_matches('.'))
    })
}

fn authority(input: &str) -> Option<&str> {
    input.split_once("://")?.1.split(['/', '?', '#']).next()
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
        || authority(input)?.contains('@')
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

#[cfg(test)]
mod tests {
    use super::*;

    const API: &str = "https://localhost";
    const APEX: &str = "example.test";
    const TOKEN: &str = "test-token";

    fn apex_policy(apex: &str, port: Option<u16>) -> Option<PreviewOriginPolicy> {
        Some(PreviewOriginPolicy::SingleSubdomainApex {
            apex_host: apex.into(),
            port,
        })
    }

    fn check_preview(backend: &HttpArtifactBackend, link: &str) -> Result<(), ArtifactError> {
        backend.check_metadata(&ArtifactMetadata {
            artifact_id: "item".into(),
            title: "Preview".into(),
            kind: ArtifactKind::Html,
            mime_type: "text/html".into(),
            version: 1,
            sha256: "0".repeat(64),
            byte_size: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            workspace_id: None,
            room_id: None,
            language: None,
            preview_url: Some(link.into()),
        })
    }

    fn preview_url(label: &str, port: &str) -> String {
        format!("https://{label}.{APEX}{port}/view?sig=opaque")
    }

    fn assert_invalid_preview(backend: &HttpArtifactBackend, link: &str) {
        assert!(
            matches!(
                check_preview(backend, link),
                Err(ArtifactError::InvalidResponse)
            ),
            "accepted {link}"
        );
    }

    #[test]
    fn single_subdomain_accepts_strict_labels_and_matching_https_ports() {
        let backend =
            HttpArtifactBackend::new_with_preview_policy(API, TOKEN, apex_policy(APEX, None))
                .unwrap();
        for label in ["a", "0", "9-report-2", &"a".repeat(63)] {
            check_preview(&backend, &preview_url(label, "")).unwrap();
        }
        check_preview(&backend, &preview_url("a", ":443")).unwrap();

        let backend =
            HttpArtifactBackend::new_with_preview_policy(API, TOKEN, apex_policy(APEX, Some(8443)))
                .unwrap();
        check_preview(&backend, &preview_url("a", ":8443")).unwrap();
        assert_invalid_preview(&backend, &preview_url("a", ""));
    }

    #[test]
    fn single_subdomain_rejects_out_of_scope_and_noncanonical_links() {
        let backend =
            HttpArtifactBackend::new_with_preview_policy(API, TOKEN, apex_policy(APEX, None))
                .unwrap();
        for (scheme, host_and_port) in [
            ("https", APEX.into()),
            ("https", format!("evil-{APEX}")),
            ("https", format!("a.{APEX}.invalid")),
            ("https", format!("a.{APEX}.")),
            ("https", format!("/a.{APEX}")),
            ("https", format!("a.{APEX}:")),
            ("http", format!("a.{APEX}")),
            ("https", format!("a.{APEX}:8443")),
        ] {
            assert_invalid_preview(&backend, &format!("{scheme}://{host_and_port}/view"));
        }
        for userinfo in ["", "user", "user:password"] {
            assert_invalid_preview(&backend, &preview_url(&format!("{userinfo}@a"), ""));
        }
        for label in [
            "",
            "a.b",
            "-a",
            "a-",
            "a_b",
            "A",
            "%61",
            "\u{00e9}",
            "xn--9ca",
            &"a".repeat(64),
        ] {
            assert_invalid_preview(&backend, &preview_url(label, ""));
        }
    }

    #[test]
    fn single_subdomain_rejects_invalid_apex_and_the_api_host() {
        let api_apex = format!("https://{APEX}./v1");
        assert!(matches!(
            HttpArtifactBackend::new_with_preview_policy(&api_apex, TOKEN, apex_policy(APEX, None)),
            Err(ArtifactError::InvalidInput(_))
        ));
        for apex in [
            "",
            "TEST",
            "https://example.test",
            "127.0.0.1",
            "localhost.",
            "a..test",
            "a_b",
            "-a",
            "xn--9ca",
        ] {
            assert!(
                matches!(
                    HttpArtifactBackend::new_with_preview_policy(
                        API,
                        TOKEN,
                        apex_policy(apex, None)
                    ),
                    Err(ArtifactError::InvalidInput(_))
                ),
                "accepted apex {apex}"
            );
        }
        // An API host that is itself a child of the apex stays excluded on any port.
        let api_child = format!("https://{host}./v1", host = format_args!("api.{APEX}"));
        let backend = HttpArtifactBackend::new_with_preview_policy(
            &api_child,
            TOKEN,
            apex_policy(APEX, Some(8443)),
        )
        .unwrap();
        assert_invalid_preview(&backend, &preview_url("api", ":8443"));
        check_preview(&backend, &preview_url("item", ":8443")).unwrap();
    }

    #[test]
    fn legacy_constructor_still_requires_an_exact_https_origin() {
        let backend =
            HttpArtifactBackend::new(API, TOKEN, Some("https://example.test:8443")).unwrap();
        let valid = "https://example.test:8443/view?sig=opaque";
        check_preview(&backend, valid).unwrap();
        // Exact remains an origin comparison, not a raw hostname comparison.
        check_preview(
            &backend,
            &format!("https://{}:8443/view", APEX.to_uppercase()),
        )
        .unwrap();
        for link in [
            "http://example.test:8443/preview",
            "https://example.test/preview",
            "https://sandbox.example.test:8443/preview",
        ] {
            assert!(
                matches!(
                    check_preview(&backend, link),
                    Err(ArtifactError::InvalidResponse)
                ),
                "accepted {link}"
            );
        }
        let unconfigured = HttpArtifactBackend::new(API, TOKEN, None).unwrap();
        assert!(matches!(
            check_preview(&unconfigured, valid),
            Err(ArtifactError::InvalidResponse)
        ));
    }
}
