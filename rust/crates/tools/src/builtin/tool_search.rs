//! ToolSearch — lets the model discover deferred tools by keyword.
//! Mirrors CC's `src/tools/ToolSearchTool/ToolSearchTool.ts`.

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

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

const MAX_ACTIVATION_SCOPES: usize = 64;
const MAX_ACTIVATED_TOOLS_PER_SCOPE: usize = 16;
type ActivationStore = RwLock<HashMap<String, HashSet<String>>>;

fn activation_store() -> &'static ActivationStore {
    static STORE: OnceLock<ActivationStore> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Mark a schema as available for subsequent model requests in this scope.
/// Returns `false` only when the per-scope activation cap rejects a new name;
/// selecting an already active tool is idempotently successful.
pub fn activate_tool(scope: &str, name: &str) -> bool {
    let mut store = activation_store().write().unwrap();
    if !store.contains_key(scope) && store.len() >= MAX_ACTIVATION_SCOPES {
        store.clear();
    }
    let activated = store.entry(scope.to_string()).or_default();
    if activated.contains(name) {
        return true;
    }
    if activated.len() >= MAX_ACTIVATED_TOOLS_PER_SCOPE {
        return false;
    }
    activated.insert(name.to_string());
    true
}

pub fn activated_tools(scope: &str) -> HashSet<String> {
    activation_store()
        .read()
        .unwrap()
        .get(scope)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolOptions;
    use nonoclaw_core::PermissionMode;
    use std::path::Path;

    #[tokio::test]
    async fn select_activates_exact_schema_for_only_the_callers_scope() {
        let scope = "tool-search-select-test";
        let other_scope = "tool-search-other-test";
        let tool = ToolSearchTool::new(vec![ToolSearchEntry {
            name: "DeferredTool".into(),
            description: "deferred test capability".into(),
            search_hint: "deferred test".into(),
        }]);
        let options = ToolOptions {
            model: "test".into(),
            permission_mode: PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let context = ToolCtx {
            cwd: Path::new("/tmp"),
            options: &options,
            cancel: &cancel,
            tool_use_id: "tool-search-call",
            task_scope: Some(scope),
            subagent: None,
            graph_runner: None,
            question: None,
            background_registry: None,
        };

        for _ in 0..2 {
            let result = tool
                .call(
                    json!({"query": "select:DeferredTool"}),
                    &context,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let payload: Value = serde_json::from_str(&result.data).unwrap();
            assert_eq!(payload["activated"], true);
            assert_eq!(payload["available_next_request"], true);
        }

        assert!(activated_tools(scope).contains("DeferredTool"));
        assert!(!activated_tools(other_scope).contains("DeferredTool"));
    }

    #[test]
    fn activation_store_enforces_per_scope_cap_without_breaking_idempotency() {
        let scope = "tool-search-cap-test";
        for index in 0..MAX_ACTIVATED_TOOLS_PER_SCOPE {
            assert!(activate_tool(scope, &format!("Deferred{index}")));
        }
        assert!(!activate_tool(scope, "OneTooMany"));
        assert!(activate_tool(scope, "Deferred0"));
        assert_eq!(activated_tools(scope).len(), MAX_ACTIVATED_TOOLS_PER_SCOPE);
    }
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
        ctx: &ToolCtx<'_>,
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
                let activated = activate_tool(ctx.task_scope(), &entry.name);
                let out = serde_json::to_string_pretty(&json!({
                    "name": entry.name,
                    "description": entry.description,
                    "activated": activated,
                    "available_next_request": activated,
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

        Ok(ToolResult::ok(format!(
            "{}\n\nCall `ToolSearch` again with `select:<exact-name>` to activate one schema for the next request.",
            lines.join("\n")
        )))
    }
}
