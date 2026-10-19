//! WebSearch tool. Pluggable backend: `SERPER_API_KEY` (google.serper.dev),
//! `BRAVE_API_KEY` (api.search.brave.com) or `DEEPSEEK_API_KEY`
//! (api.deepseek.com `/v1/tools/search`). Returns top results as text
//! (title, URL, snippet). Without a key, returns guidance.

use crate::tool::{Tool, ToolCtx, ToolResult};
use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "Web search tool. Searches the web and returns top results (title, URL, snippet).\n\nConfigure via SERPER_API_KEY, BRAVE_API_KEY or DEEPSEEK_API_KEY environment variable; the first configured key wins unless WEBSEARCH_BACKEND names one explicitly. Without a key, the tool returns a guidance message so you can use your own knowledge.";

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "WebSearch"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Search the web."
    }
    fn search_hint(&self) -> Option<&'static str> {
        Some("search the web internet lookup")
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string","description":"Search query"}},"required":["query"]})
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionResult::allow()
    }
    async fn call(
        &self,
        input: Value,
        _ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let q = input["query"].as_str().ok_or_else(|| Error::Tool {
            tool: "WebSearch".into(),
            message: "missing query".into(),
        })?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // `WEBSEARCH_BACKEND` forces a backend when several keys are set;
        // otherwise the first configured key wins (serper, brave, deepseek).
        match std::env::var("WEBSEARCH_BACKEND").as_deref() {
            Ok("serper") => {
                let key = env_key("SERPER_API_KEY")?;
                return serper(&key, q).await;
            }
            Ok("brave") => {
                let key = env_key("BRAVE_API_KEY")?;
                return brave(&key, q).await;
            }
            Ok("deepseek") => {
                let key = env_key("DEEPSEEK_API_KEY")?;
                return deepseek(&key, q).await;
            }
            _ => {}
        }
        if let Ok(key) = std::env::var("SERPER_API_KEY") {
            return serper(&key, q).await;
        }
        if let Ok(key) = std::env::var("BRAVE_API_KEY") {
            return brave(&key, q).await;
        }
        if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
            return deepseek(&key, q).await;
        }
        Ok(ToolResult::ok("WebSearch: set SERPER_API_KEY, BRAVE_API_KEY or DEEPSEEK_API_KEY env var for live search; I'll answer from my knowledge."))
    }
}

/// Read a backend key, reporting a forced-but-unconfigured backend clearly
/// instead of silently falling back to another provider.
fn env_key(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| Error::Tool {
        tool: "WebSearch".into(),
        message: format!("WEBSEARCH_BACKEND={name} is not set"),
    })
}

async fn serper(key: &str, q: &str) -> Result<ToolResult> {
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Tool {
            tool: "WebSearch".into(),
            message: format!("client: {e}"),
        })?;
    let resp: Value = c
        .post("https://google.serper.dev/search")
        .header("X-API-KEY", key)
        .json(&json!({"q":q}))
        .send()
        .await
        .map_err(|e| Error::Tool {
            tool: "WebSearch".into(),
            message: format!("req: {e}"),
        })?
        .json()
        .await
        .map_err(|e| Error::Tool {
            tool: "WebSearch".into(),
            message: format!("parse: {e}"),
        })?;
    let mut out = String::new();
    if let Some(arr) = resp.get("organic").and_then(|v| v.as_array()) {
        for (i, r) in arr.iter().take(10).enumerate() {
            let t = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let u = r.get("link").and_then(|v| v.as_str()).unwrap_or("");
            let sn = r.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
            out.push_str(&format!("{}. {}\n  {}\n  {}\n\n", i + 1, t, u, sn));
        }
    }
    if out.is_empty() {
        Ok(ToolResult::ok("No results."))
    } else {
        Ok(ToolResult::ok(out))
    }
}

async fn brave(key: &str, q: &str) -> Result<ToolResult> {
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Tool {
            tool: "WebSearch".into(),
            message: format!("client: {e}"),
        })?;
    let resp: Value = c
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .query(&[("q", q)])
        .send()
        .await
        .map_err(|e| Error::Tool {
            tool: "WebSearch".into(),
            message: format!("req: {e}"),
        })?
        .json()
        .await
        .map_err(|e| Error::Tool {
            tool: "WebSearch".into(),
            message: format!("parse: {e}"),
        })?;
    let mut out = String::new();
    if let Some(arr) = resp
        .get("web")
        .and_then(|v| v.get("results"))
        .and_then(|v| v.as_array())
    {
        for (i, r) in arr.iter().take(10).enumerate() {
            let t = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let u = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let sn = r.get("description").and_then(|v| v.as_str()).unwrap_or("");
            out.push_str(&format!("{}. {}\n  {}\n  {}\n\n", i + 1, t, u, sn));
        }
    }
    if out.is_empty() {
        Ok(ToolResult::ok("No results."))
    } else {
        Ok(ToolResult::ok(out))
    }
}

/// DeepSeek's self-hosted search (`POST /v1/tools/search`). Billed per
/// successful request (5 CNY / 1000), 5 QPS per account.
async fn deepseek(key: &str, q: &str) -> Result<ToolResult> {
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| Error::Tool {
            tool: "WebSearch".into(),
            message: format!("client: {e}"),
        })?;
    let resp = c
        .post("https://api.deepseek.com/v1/tools/search")
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"query": q, "count": 10}))
        .send()
        .await
        .map_err(|e| Error::Tool {
            tool: "WebSearch".into(),
            message: format!("req: {e}"),
        })?;
    let status = resp.status();
    let resp: Value = resp.json().await.map_err(|e| Error::Tool {
        tool: "WebSearch".into(),
        message: format!("parse: {e}"),
    })?;
    if !status.is_success() {
        // Surface the provider message (401 key, 402 balance, 429 QPS) inline.
        let msg = resp
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("request failed");
        return Ok(ToolResult::ok(format!(
            "WebSearch: DeepSeek HTTP {}: {msg}",
            status.as_u16()
        )));
    }
    let mut out = String::new();
    format_deepseek_results(&resp, &mut out);
    if out.is_empty() {
        Ok(ToolResult::ok("No results."))
    } else {
        Ok(ToolResult::ok(out))
    }
}

fn format_deepseek_results(resp: &Value, out: &mut String) {
    if let Some(arr) = resp.get("results").and_then(|v| v.as_array()) {
        for (i, r) in arr.iter().take(10).enumerate() {
            let t = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let u = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            // `summary` (~600 chars) is meant for model consumption; fall back
            // to it when the short `snippet` is missing.
            let sn = r
                .get("snippet")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| r.get("summary").and_then(|v| v.as_str()))
                .unwrap_or("");
            out.push_str(&format!("{}. {}\n  {}\n  {}\n\n", i + 1, t, u, sn));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_results_prefer_snippet_then_summary() {
        let resp = json!({"results": [
            {"title": "A", "url": "https://a", "snippet": "short", "summary": "long"},
            {"title": "B", "url": "https://b", "snippet": "", "summary": "fallback"}
        ]});
        let mut out = String::new();
        format_deepseek_results(&resp, &mut out);
        assert!(out.contains("1. A\n  https://a\n  short"));
        assert!(out.contains("2. B\n  https://b\n  fallback"));
    }

    #[test]
    fn deepseek_empty_results_yields_nothing() {
        let mut out = String::new();
        format_deepseek_results(&json!({"results": []}), &mut out);
        assert!(out.is_empty());
    }
}
