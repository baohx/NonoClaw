//! WebSearch tool. Pluggable backend: `SERPER_API_KEY` (google.serper.dev)
//! or `BRAVE_API_KEY` (api.search.brave.com). Returns top results as text
//! (title, URL, snippet). Without a key, returns guidance.

use crate::tool::{Tool, ToolCtx, ToolResult};
use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "Web search tool. Searches the web and returns top results (title, URL, snippet).\n\nConfigure via SERPER_API_KEY or BRAVE_API_KEY environment variable. Without a key, the tool returns a guidance message so you can use your own knowledge.";

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
        if let Ok(key) = std::env::var("SERPER_API_KEY") {
            return serper(&key, q).await;
        }
        if let Ok(key) = std::env::var("BRAVE_API_KEY") {
            return brave(&key, q).await;
        }
        Ok(ToolResult::ok("WebSearch: set SERPER_API_KEY or BRAVE_API_KEY env var for live search; I'll answer from my knowledge."))
    }
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
