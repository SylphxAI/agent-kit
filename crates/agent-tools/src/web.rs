//! Pure-Rust web utilities for SSRF IP validation, HTML-to-Markdown extraction,
//! and pluggable SSRF-guarded HTTP fetching.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

/// Default maximum response body size read by [`ssrf_safe_fetch`].
pub const DEFAULT_MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
/// Default request timeout for [`ssrf_safe_fetch`].
pub const DEFAULT_FETCH_TIMEOUT_MS: u64 = 15_000;

/// Returns `true` if `ip` is a global unicast address safe for outbound HTTP fetches,
/// or `false` if it belongs to loopback, private, link-local, CGNAT, multicast,
/// broadcast, documentation, benchmarking, or unspecified ranges.
pub fn is_global_unicast_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 0b0100_0000) // CGNAT 100.64.0.0/10
                || (v4.octets()[0] == 198 && (v4.octets()[1] & 0xfe) == 18)) // Benchmarking 198.18.0.0/15
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_global_unicast_ip(IpAddr::V4(mapped));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // Unique local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // Link-local fe80::/10
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8)) // Documentation 2001:db8::/32
        }
    }
}

/// Validate a URL's scheme (`http`/`https`) and verify that every resolved IP
/// address is globally routable ([`is_global_unicast_ip`]).
pub fn validate_public_url(raw_url: &str) -> Result<(String, u16), String> {
    let trimmed = raw_url.trim();
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("URL must include http:// or https:// scheme: {raw_url}"))?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80u16,
        "https" => 443u16,
        other => return Err(format!("unsupported URL scheme `{other}`")),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, hp)| hp)
        .trim();
    if host_port.is_empty() {
        return Err("URL missing host".into());
    }
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (addr, tail) = bracketed
            .split_once(']')
            .ok_or_else(|| format!("malformed IPv6 host in URL: {raw_url}"))?;
        let port = if let Some(p) = tail.strip_prefix(':') {
            p.parse::<u16>()
                .map_err(|_| format!("invalid port in URL: {raw_url}"))?
        } else {
            default_port
        };
        (addr.to_owned(), port)
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        let port = p
            .parse::<u16>()
            .map_err(|_| format!("invalid port in URL: {raw_url}"))?;
        (h.to_owned(), port)
    } else {
        (host_port.to_owned(), default_port)
    };
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return Err(format!("SSRF guard rejected loopback host `{host}`"));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_global_unicast_ip(ip) {
            return Err(format!("SSRF guard rejected non-global IP `{ip}`"));
        }
        return Ok((host, port));
    }
    let addrs: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for `{host}`: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("DNS resolution returned no addresses for `{host}`"));
    }
    for sock in &addrs {
        if !is_global_unicast_ip(sock.ip()) {
            return Err(format!(
                "SSRF guard rejected non-global resolved IP `{}` for `{host}`",
                sock.ip()
            ));
        }
    }
    Ok((host, port))
}

/// Request payload for [`WebBackend::fetch`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WebFetchRequest {
    pub url: String,
    #[serde(default)]
    pub max_bytes: Option<usize>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub raw: bool,
}

/// Response returned by [`WebBackend::fetch`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebFetchResponse {
    pub url: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub content_type: String,
    pub content: String,
    pub truncated: bool,
}

/// Request payload for [`WebBackend::search`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WebSearchRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Pluggable web backend mounted under the `web.*` tool namespace.
#[async_trait]
pub trait WebBackend: Send + Sync {
    async fn fetch(&self, req: WebFetchRequest) -> Result<WebFetchResponse, String>;

    async fn search(&self, _req: WebSearchRequest) -> Result<serde_json::Value, String> {
        Err("web.search provider is not configured on this runtime".into())
    }
}

/// Default SSRF-guarded HTTP [`WebBackend`] (validates global unicast IPs before
/// connecting, refuses redirects to non-global hosts, bounds response bytes, and
/// converts HTML into Markdown).
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultWebBackend;

/// Perform one SSRF-guarded HTTP(S) fetch synchronously.
pub fn ssrf_safe_fetch(req: &WebFetchRequest) -> Result<WebFetchResponse, String> {
    validate_public_url(&req.url)?;
    let timeout = Duration::from_millis(
        req.timeout_ms
            .unwrap_or(DEFAULT_FETCH_TIMEOUT_MS)
            .clamp(500, 60_000),
    );
    let max_bytes = req
        .max_bytes
        .unwrap_or(DEFAULT_MAX_FETCH_BYTES)
        .clamp(256, 8 * 1024 * 1024);
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent("agent-tools")
        .build();
    let agent: ureq::Agent = config.into();
    let mut current_url = req.url.trim().to_owned();
    for _ in 0..=5 {
        validate_public_url(&current_url)?;
        let mut resp = agent
            .get(&current_url)
            .call()
            .map_err(|e| format!("web fetch failed for {current_url}: {e}"))?;
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            if loc.starts_with("http://") || loc.starts_with("https://") {
                current_url = loc.to_owned();
                continue;
            }
            return Err(format!("refusing relative redirect to `{loc}`"));
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let mut buf = Vec::new();
        resp.body_mut()
            .as_reader()
            .take((max_bytes + 1) as u64)
            .read_to_end(&mut buf)
            .map_err(|e| format!("failed reading response body: {e}"))?;
        let truncated = buf.len() > max_bytes;
        if truncated {
            buf.truncate(max_bytes);
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (title, content) = if req.raw {
            (None, text)
        } else {
            html_to_markdown(&text)
        };
        return Ok(WebFetchResponse {
            url: current_url,
            status,
            title,
            content_type,
            content,
            truncated,
        });
    }
    Err("too many redirects".into())
}

#[async_trait]
impl WebBackend for DefaultWebBackend {
    async fn fetch(&self, req: WebFetchRequest) -> Result<WebFetchResponse, String> {
        tokio::task::spawn_blocking(move || ssrf_safe_fetch(&req))
            .await
            .map_err(|e| format!("web fetch task failed: {e}"))?
    }
}

/// Extracts an optional `<title>` and converts an HTML document into clean Markdown,
/// stripping `<script>`, `<style>`, `<noscript>`, `<svg>`, and `<head>` blocks.
pub fn html_to_markdown(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim();
    if !trimmed.contains('<')
        || (!trimmed.to_ascii_lowercase().contains("<html")
            && !trimmed.to_ascii_lowercase().contains("<body")
            && !trimmed.to_ascii_lowercase().contains("<p")
            && !trimmed.to_ascii_lowercase().contains("<div")
            && !trimmed.to_ascii_lowercase().contains("<h1")
            && !trimmed.to_ascii_lowercase().contains("<h2")
            && !trimmed.to_ascii_lowercase().contains("<article")
            && !trimmed.to_ascii_lowercase().contains("<main"))
    {
        return (None, trimmed.to_string());
    }

    let title = extract_tag_inner(trimmed, "title").map(|t| decode_html_entities(t.trim()));
    let mut working = strip_tag_blocks(trimmed, "script");
    working = strip_tag_blocks(&working, "style");
    working = strip_tag_blocks(&working, "noscript");
    working = strip_tag_blocks(&working, "svg");
    working = strip_tag_blocks(&working, "head");

    let mut out = String::with_capacity(working.len());
    let mut in_tag = false;
    let mut tag_buf = String::new();
    for ch in working.chars() {
        if ch == '<' {
            in_tag = true;
            tag_buf.clear();
        } else if ch == '>' && in_tag {
            in_tag = false;
            let tag_lower = tag_buf
                .trim()
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            let is_closing = tag_buf.trim().starts_with('/');
            match tag_lower.as_str() {
                "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "tr"
                | "br" | "hr" | "blockquote" | "pre" => {
                    out.push('\n');
                }
                "h1" => {
                    out.push_str(if is_closing { "\n" } else { "\n# " });
                }
                "h2" => {
                    out.push_str(if is_closing { "\n" } else { "\n## " });
                }
                "h3" | "h4" | "h5" | "h6" => {
                    out.push_str(if is_closing { "\n" } else { "\n### " });
                }
                "li" => {
                    out.push_str(if is_closing { "\n" } else { "\n- " });
                }
                _ => {}
            }
        } else if in_tag {
            tag_buf.push(ch);
        } else {
            out.push(ch);
        }
    }

    let decoded = decode_html_entities(&out);
    let mut lines = Vec::new();
    let mut blank_run = 0usize;
    for raw_line in decoded.lines() {
        let collapsed = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !lines.is_empty() {
                lines.push(String::new());
            }
        } else {
            blank_run = 0;
            lines.push(collapsed);
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    (title.filter(|t| !t.is_empty()), lines.join("\n"))
}

fn extract_tag_inner<'a>(html: &'a str, tag: &str) -> Option<&'a str> {
    let lower = html.to_ascii_lowercase();
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}>");
    let start = lower.find(&open_pat)?;
    let gt = start + lower[start..].find('>')? + 1;
    let end = gt + lower[gt..].find(&close_pat)?;
    Some(&html[gt..end])
}

fn strip_tag_blocks(html: &str, tag: &str) -> String {
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    let lower = html.to_ascii_lowercase();
    while let Some(rel_start) = lower[cursor..].find(&open_pat) {
        let abs_start = cursor + rel_start;
        out.push_str(&html[cursor..abs_start]);
        if let Some(rel_end) = lower[abs_start..].find(&close_pat) {
            cursor = abs_start + rel_end + close_pat.len();
        } else {
            cursor = html.len();
            break;
        }
    }
    out.push_str(&html[cursor..]);
    out
}

/// Decodes common HTML character entities into UTF-8 text.
pub fn decode_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_unicast_ip_classifier_rejects_local_and_private_ranges() {
        for blocked in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = blocked.parse().unwrap();
            assert!(
                !is_global_unicast_ip(ip),
                "expected {blocked} to be rejected"
            );
        }
        for public in [
            "93.184.216.34",
            "1.1.1.1",
            "2606:2800:220:1:248:1893:25c8:1946",
        ] {
            let ip: IpAddr = public.parse().unwrap();
            assert!(is_global_unicast_ip(ip), "expected {public} to be accepted");
        }
    }

    #[test]
    fn validate_public_url_blocks_loopback_private_and_non_http_schemes() {
        assert!(validate_public_url("http://127.0.0.1:8080/secret").is_err());
        assert!(validate_public_url("http://localhost/admin").is_err());
        assert!(validate_public_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(validate_public_url("file:///etc/passwd").is_err());
        assert!(validate_public_url("https://1.1.1.1/").is_ok());
    }

    #[test]
    fn html_to_markdown_extracts_title_and_clean_body() {
        let html = r#"
            <html>
              <head><title>Docs &amp; Guide</title><style>body{color:red}</style></head>
              <body>
                <script>console.log("ignore")</script>
                <h1>Heading One</h1>
                <p>First paragraph with <strong>bold</strong> text.</p>
                <ul><li>Alpha</li><li>Beta</li></ul>
              </body>
            </html>
        "#;
        let (title, md) = html_to_markdown(html);
        assert_eq!(title.as_deref(), Some("Docs & Guide"));
        assert!(md.contains("# Heading One"));
        assert!(md.contains("First paragraph with bold text."));
        assert!(md.contains("- Alpha"));
        assert!(!md.contains("console.log"));
    }
}
