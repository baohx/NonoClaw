//! ToolSearch — lets the model discover deferred tools by keyword.
//! Mirrors CC's `src/tools/ToolSearchTool/ToolSearchTool.ts`.

use async_trait::async_trait;
use nonoclaw_core::{PermissionResult, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

/// Snapshot of a tool for keyword search.
#[derive(Clone)]
pub struct ToolSearchEntry {
    pub name: String,
    pub description: String,
    pub search_hint: String,
}

pub struct ToolSearchTool {
    entries: Vec<ToolSearchEntry>,
}

impl ToolSearchTool {
    pub fn new(entries: Vec<ToolSearchEntry>) -> Self {
        ToolSearchTool { entries }
    }
}

#[derive(Debug, Deserialize)]
struct ToolSearchInput {
    query: String,
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "ToolSearch"
    }
    fn description(&self) -> &str {
        "Search for available tools by keyword."
    }
    fn prompt(&self) -> &str {
        "Search for tools by keyword. Use `select:<tool-name>` for exact match."
    }
    fn search_hint(&self) -> Option<&str> {
        Some("find tools by keyword search")
    }
    fn should_defer(&self) -> bool {
        false
    }
    fn aliases(&self) -> &[&str] {
        &[]
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords. Use `select:<tool-name>` for exact match."
                }
            },
            "required": ["query"]
        })
    }

    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionResult::allow()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let parsed: ToolSearchInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::error(format!("invalid input: {e}"))),
        };

        let query = parsed.query.trim();

        // `select:<name>` — exact tool lookup.
        if let Some(name) = query.strip_prefix("select:").map(|s| s.trim()) {
            if let Some(entry) = self.entries.iter().find(|e| e.name == name) {
                let out = serde_json::to_string_pretty(&json!({
                    "name": entry.name,
                    "description": entry.description,
                }))
                .unwrap_or_default();
                return Ok(ToolResult::ok(out));
            }
            return Ok(ToolResult::ok(format!("no tool named '{name}' found")));
        }

        // Keyword search.
        let tokens: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        let mut scored: Vec<(&ToolSearchEntry, i32)> = self
            .entries
            .iter()
            .map(|entry| {
                let name = entry.name.to_lowercase();
                let desc = entry.description.to_lowercase();
                let hint = entry.search_hint.to_lowercase();
                let mut score: i32 = 0;
                for tok in &tokens {
                    if name == *tok {
                        score += 100;
                    } else if name.contains(tok) {
                        score += 50;
                    }
                    if hint.contains(tok) {
                        score += 30;
                    }
                    if desc.contains(tok) {
                        score += 10;
                    }
                }
                (entry, score)
            })
            .filter(|(_, s)| *s > 0)
            .collect();

        scored.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        scored.truncate(10);

        if scored.is_empty() {
            return Ok(ToolResult::ok(
                "No matching tools found. Try different keywords.",
            ));
        }

        let lines: Vec<String> = scored
            .into_iter()
            .map(|(e, _)| format!("- **{}**: {}", e.name, e.description))
            .collect();

        Ok(ToolResult::ok(lines.join("\n")))
    }
}
