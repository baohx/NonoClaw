//! WebFetch tool. Mirrors `src/tools/WebFetchTool/`. Fetches a URL over HTTP
//! and returns a text rendering of the body (tags stripped, entities decoded,
//! whitespace collapsed). A full readability/markdown converter is deferred;
//! this is a faithful-enough approximation for Phase 2.

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
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(Error::Tool {
                tool: "WebFetch".into(),
                message: format!("url must be absolute http(s): {url}"),
            });
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("nonoclaw-webfetch/0.1 (+https://claude.com/claude-code)")
            .build()
            .map_err(|e| Error::Tool {
                tool: "WebFetch".into(),
                message: format!("http client build failed: {e}"),
            })?;

        let resp = client.get(url).send().await.map_err(|e| Error::Tool {
            tool: "WebFetch".into(),
            message: format!("request failed: {e}"),
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
            bytes.truncate(MAX_BYTES);
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
            subagent: None,
            question: None,
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
