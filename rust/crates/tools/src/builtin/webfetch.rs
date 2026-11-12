//! WebFetch tool. Mirrors `src/tools/WebFetchTool/`. Fetches a URL over HTTP
//! and returns a text rendering of the body (tags stripped, entities decoded,
//! whitespace collapsed). A full readability/markdown converter is deferred;
//! this is a faithful-enough approximation for Phase 2.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Fetches a URL over HTTP(S) and returns the page content as text.\n\nUsage:\n- Provide an absolute URL (`url`) and an optional `prompt` describing what to extract.\n- HTML is converted to plain text (tags removed, entities decoded); very large pages are truncated.\n- Prefer this for reading documentation, APIs, or articles. For search, request WebSearch instead.\n- The user-agent identifies this as the NonoClaw WebFetch tool.";

const MAX_BYTES: usize = 2_000_000; // 2 MB response cap before parsing
const MAX_CHARS: usize = 30_000;

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "WebFetch"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Fetch a URL and return its content as text."
    }
    fn search_hint(&self) -> Option<&'static str> {
        Some("fetch a url read a web page http")
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type":"string","description":"The absolute URL to fetch"},
                "prompt": {"type":"string","description":"What to extract or focus on (optional)"}
            },
            "required": ["url"]
        })
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, input: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        // Network egress is an open-world action; ask in interactive sessions,
        // but the engine auto-allows read-only tools in default headless.
        let _ = input;
        PermissionResult::allow()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let url = input["url"].as_str().ok_or_else(|| Error::Tool {
            tool: "WebFetch".into(),
            message: "missing required field `url`".into(),
        })?;
        let focus = input["prompt"].as_str().unwrap_or("");

        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let mut current = reqwest::Url::parse(url).map_err(|e| Error::Tool {
            tool: "WebFetch".into(),
            message: format!("invalid url: {e}"),
        })?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("nonoclaw-webfetch/0.1 (+https://claude.com/claude-code)")
            // Redirects are followed manually so every hop re-passes the SSRF
            // guard (scheme, literal IP, resolved IPs) below.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Tool {
                tool: "WebFetch".into(),
                message: format!("http client build failed: {e}"),
            })?;

        const MAX_REDIRECTS: usize = 5;
        let mut resp = None;
        for _ in 0..=MAX_REDIRECTS {
            validate_url(&current)?;
            ensure_resolved_ips_public(&current).await?;
            let r = client.get(current.clone()).send().await.map_err(|e| Error::Tool {
                tool: "WebFetch".into(),
                message: format!("request failed: {e}"),
            })?;
            if r.status().is_redirection() {
                let loc = r
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok());
                match loc {
                    Some(loc) => {
                        current = current.join(loc).map_err(|e| Error::Tool {
                            tool: "WebFetch".into(),
                            message: format!("invalid redirect location `{loc}`: {e}"),
                        })?;
                        continue;
                    }
                    None => {
                        resp = Some(r);
                        break;
                    }
                }
            }
            resp = Some(r);
            break;
        }
        let resp = resp.ok_or_else(|| Error::Tool {
            tool: "WebFetch".into(),
            message: format!("too many redirects (max {MAX_REDIRECTS})"),
        })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Tool {
                tool: "WebFetch".into(),
                message: format!("HTTP {status}"),
            });
        }

        // Read the body (capped to MAX_BYTES after parsing). reqwest handles
        // chunked/gzip transparently.
        let mut bytes = resp.text().await.map_err(|e| Error::Tool {
            tool: "WebFetch".into(),
            message: format!("read failed: {e}"),
        })?;
        if bytes.len() > MAX_BYTES {
            // MAX_BYTES may not align on a UTF-8 char boundary.
            // floor_char_boundary returns the nearest boundary ≤ MAX_BYTES.
            let boundary = bytes.floor_char_boundary(MAX_BYTES);
            bytes.truncate(boundary);
        }

        let text = html_to_text(&bytes);
        let mut text = truncate_chars(&text, MAX_CHARS);
        if !focus.is_empty() {
            text = format!("Focus: {focus}\n\n{text}");
        }
        Ok(ToolResult::ok(text))
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push_str("\n…[truncated]");
    t
}

fn ssrf_error(message: String) -> Error {
    Error::Tool {
        tool: "WebFetch".into(),
        message,
    }
}

/// True when the IP is not reachable from the public internet (loopback,
/// private, link-local, unique-local, multicast, broadcast, unspecified).
/// Fetching such an address would turn WebFetch into a network probe of the
/// host's own environment (SSRF), so it is refused.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified())
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                // IPv4-mapped (::ffff:a.b.c.d) — judge by the embedded v4.
                return is_public_ip(IpAddr::V4(v4));
            }
            !(v6.is_loopback()
                || is_unique_local_v6(v6)
                || is_link_local_v6(v6)
                || v6.is_unspecified()
                || v6.is_multicast())
        }
    }
}

/// std has no `Ipv6Addr::is_unique_local`; the fc00::/7 range is `fc00::`
/// masked to 7 bits (fc00::/8 plus fd00::/8).
fn is_unique_local_v6(ip: std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// fe80::/10 (link-local) — first 10 bits equal 1111111010.
fn is_link_local_v6(ip: std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// Scheme/host sanity on the URL itself. Literal-IP hosts are checked here
/// (cheap, no DNS); hostname hosts are checked after resolution by the caller.
fn validate_url(url: &reqwest::Url) -> Result<()> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(ssrf_error(format!(
                "url scheme must be http(s), got `{other}`"
            )))
        }
    }
    let host = url
        .host_str()
        .ok_or_else(|| ssrf_error("url has no host".into()))?;
    // Brackets are url::Host's serialization for IPv6 literals.
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        if !is_public_ip(ip) {
            return Err(ssrf_error(format!(
                "refusing to fetch non-public address `{host}` (SSRF guard)"
            )));
        }
    }
    Ok(())
}

/// Resolve the URL's hostname and refuse when any resolved address is
/// non-public. Blocks DNS-rebinding-into-localhost/private attacks; `localhost`
/// itself resolves to loopback and is refused.
async fn ensure_resolved_ips_public(url: &reqwest::Url) -> Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| ssrf_error("url has no host".into()))?;
    if host
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .is_ok()
    {
        return Ok(()); // literal IP — already validated in validate_url
    }
    let addrs = tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(80)))
        .await
        .map_err(|e| Error::Tool {
            tool: "WebFetch".into(),
            message: format!("dns lookup failed for `{host}`: {e}"),
        })?;
    let mut saw_any = false;
    for addr in addrs {
        saw_any = true;
        if !is_public_ip(addr.ip()) {
            return Err(ssrf_error(format!(
                "`{host}` resolves to non-public address {} (SSRF guard)",
                addr.ip()
            )));
        }
    }
    if !saw_any {
        return Err(ssrf_error(format!("`{host}` resolved to no addresses")));
    }
    Ok(())
}

/// Convert HTML to plain text: drop script/style/etc blocks, strip tags,
/// insert newlines for block elements, decode common entities, collapse
/// whitespace. A best-effort approximation — not a full parser.
pub fn html_to_text(html: &str) -> String {
    let chars: Vec<char> = html.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if chars[i] == '<' {
            // optional '/'
            let mut j = i + 1;
            let closing = j < n && chars[j] == '/';
            if closing {
                j += 1;
            }
            let name_start = j;
            while j < n && chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            let name: String = chars[name_start..j].iter().collect();
            let name_lower = name.to_ascii_lowercase();
            // advance to '>'
            while i < n && chars[i] != '>' {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            // skip block-level content tags entirely
            if !closing
                && matches!(
                    name_lower.as_str(),
                    "script" | "style" | "noscript" | "template" | "svg" | "head"
                )
            {
                let close_marker = format!("</{name_lower}");
                let mut k = i;
                let mut matched = false;
                while k + close_marker.len() <= n {
                    let win: String = chars[k..k + close_marker.len()].iter().collect();
                    if win.eq_ignore_ascii_case(&close_marker) {
                        matched = true;
                        break;
                    }
                    k += 1;
                }
                if matched {
                    i = k + close_marker.len();
                    while i < n && chars[i] != '>' {
                        i += 1;
                    }
                    if i < n {
                        i += 1;
                    }
                } else {
                    i = n;
                }
            } else if matches!(
                name_lower.as_str(),
                "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "hr"
            ) {
                out.push('\n');
            } else {
                out.push(' ');
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    let decoded = decode_entities(&out);
    collapse_ws(&decoded)
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    let mut space_run = false;
    for c in s.chars() {
        if c == '\n' {
            space_run = false;
            blank += 1;
            if blank <= 2 {
                out.push('\n');
            }
        } else if c == ' ' || c == '\t' || c == '\r' {
            space_run = true;
        } else {
            if space_run {
                if !out.ends_with('\n') && !out.is_empty() {
                    out.push(' ');
                }
                space_run = false;
            }
            blank = 0;
            out.push(c);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_decodes_entities() {
        let html = "<html><head><title>x</title></head><body><h1>Title</h1><p>A &amp; B &lt;c&gt;</p><script>bad()</script></body></html>";
        let t = html_to_text(html);
        assert!(!t.contains("bad()"));
        assert!(!t.contains("script"));
        assert!(t.contains("Title"));
        assert!(t.contains("A & B <c>"));
    }

    #[test]
    fn block_elements_split_lines() {
        let html = "<p>one</p><p>two</p><div>three</div>";
        let t = html_to_text(html);
        assert!(t.contains("one"));
        assert!(t.contains("two"));
        assert!(t.contains("three"));
        // each block on its own line
        assert!(t.lines().count() >= 3);
    }

    #[test]
    fn collapses_whitespace() {
        let html = "<p>a    b\n\n\n  c</p>";
        let t = html_to_text(html);
        assert!(!t.contains("  ")); // no double spaces
    }

    #[test]
    fn self_closing_and_attrs() {
        let html = r#"<a href="u">link</a><br/><img src="x">"#;
        let t = html_to_text(html);
        assert!(t.contains("link"));
        assert!(!t.contains("href"));
    }

    #[test]
    fn ssrf_guard_rejects_private_and_loopback_literal_ips() {
        for bad in [
            "http://127.0.0.1/x",
            "http://127.0.0.1:8765/x",
            "http://10.0.0.1/x",
            "http://192.168.1.1/x",
            "http://172.16.0.1/x",
            "http://169.254.169.254/latest/meta-data", // cloud metadata
            "http://0.0.0.0/x",
            "http://[::1]/x",
            "http://[fe80::1]/x",
            "http://[fd00::1]/x",
            "http://[::ffff:10.0.0.1]/x",
        ] {
            let url = reqwest::Url::parse(bad).unwrap();
            let err = validate_url(&url).expect_err(bad);
            let msg = match err {
                Error::Tool { message, .. } => message,
                other => panic!("{bad}: expected Tool error, got {other:?}"),
            };
            assert!(msg.contains("SSRF"), "{bad}: {msg}");
        }
    }

    #[test]
    fn ssrf_guard_allows_public_literal_ips_and_rejects_schemes() {
        let ok = reqwest::Url::parse("http://93.184.216.34/").unwrap();
        assert!(validate_url(&ok).is_ok());
        let ok6 = reqwest::Url::parse("https://[2606:2800:220:1:248:1893:25c8:1946]/").unwrap();
        assert!(validate_url(&ok6).is_ok());

        for bad in ["file:///etc/passwd", "ftp://example.com/x", "gopher://x"] {
            let url = reqwest::Url::parse(bad).unwrap();
            assert!(validate_url(&url).is_err(), "{bad} should be rejected");
        }
    }

    #[tokio::test]
    async fn ssrf_guard_rejects_localhost_by_resolution() {
        // `localhost` resolves to 127.0.0.1/::1 on every normal host; this
        // exercises the resolve-then-check path without external DNS.
        let url = reqwest::Url::parse("http://localhost:1/").unwrap();
        let err = ensure_resolved_ips_public(&url).await.expect_err("localhost");
        match err {
            Error::Tool { message, .. } => assert!(message.contains("SSRF"), "{message}"),
            other => panic!("expected Tool error, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore] // network; run with: cargo test -p nonoclaw-tools webfetch -- --ignored
    async fn fetches_a_real_url() {
        use crate::tool::{Tool, ToolCtx, ToolOptions};
        let tool = WebFetchTool;
        let opts = ToolOptions {
            model: "x".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let cwd = std::path::Path::new("/tmp");
        let ctx = ToolCtx {
            cwd,
            options: &opts,
            cancel: &cancel,
            tool_use_id: "webfetch-test-call",
            task_scope: Some("webfetch-test"),
            subagent: None,
            graph_runner: None,
            question: None,
            background_registry: None,
        };
        let res = tool
            .call(
                json!({"url": "https://example.com"}),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .expect("fetch should succeed");
        assert!(res.data.contains("Example Domain"), "got: {}", res.data);
    }
}
