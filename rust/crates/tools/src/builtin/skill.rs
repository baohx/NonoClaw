//! Skill — load a skill's full instructions on demand (progressive disclosure).
//!
//! Skill *metadata* (name + description + when-to-use) lives in the cached
//! system prompt; the full *body* is loaded via this tool only when needed,
//! returning as a tool result in the uncached message tail. This keeps the
//! cached prefix small and byte-stable across turns — the body never sits in
//! the cacheable prefix, so loading a skill never invalidates the prompt cache.

use std::sync::Arc;

use async_trait::async_trait;
use nonoclaw_core::{PermissionResult, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillSearchEntry {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
}

/// Live read-access to skill bodies without coupling the `tools` crate to the
/// engine (which owns `SkillsManager`). Implemented in the engine crate; the
/// tool holds an `Arc<dyn SkillSource>`.
pub trait SkillSource: Send + Sync {
    /// Full, untruncated body for `name`, with `$ARGUMENTS` / `${...}`
    /// substitution applied. `None` if the skill does not exist.
    fn render_skill_body(&self, name: &str, args: &str, session_id: &str) -> Option<String>;

    /// One-line description for a skill (used in error hints). `None` if unknown.
    fn skill_description(&self, name: &str) -> Option<String>;

    /// Search all discoverable skills without loading their bodies.
    fn search_skills(&self, query: &str, limit: usize) -> Vec<SkillSearchEntry>;
}

/// Tool that loads a skill's full body by name.
pub struct SkillTool {
    source: Arc<dyn SkillSource>,
}

impl SkillTool {
    pub fn new(source: Arc<dyn SkillSource>) -> Self {
        SkillTool { source }
    }
}

#[derive(Debug, Deserialize)]
struct SkillInput {
    name: String,
    #[serde(default)]
    args: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }
    fn description(&self) -> &str {
        "Load the full instructions (body) of a skill by name. Call this when the \
         Available Skills metadata indicates a skill applies to the current task, \
         before following its steps."
    }
    fn prompt(&self) -> &str {
        "Load a skill's full instructions by name. Use it when a skill listed under \
         Available Skills applies to the current task."
    }
    fn search_hint(&self) -> Option<&str> {
        Some("load skill instructions by name")
    }
    fn should_defer(&self) -> bool {
        false
    }
    fn max_result_size_chars(&self) -> usize {
        12_000
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
                "name": {
                    "type": "string",
                    "description": "Skill name exactly as listed under Available Skills."
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments to substitute into the skill body.",
                    "default": ""
                }
            },
            "required": ["name"]
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
        let parsed: SkillInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::error(format!("invalid input: {e}"))),
        };
        match self
            .source
            .render_skill_body(&parsed.name, &parsed.args, ctx.task_scope())
        {
            Some(body) => Ok(ToolResult::ok(body)),
            None => {
                let hint = self
                    .source
                    .skill_description(&parsed.name)
                    .map(|d| format!(" (hint: {d})"))
                    .unwrap_or_default();
                Ok(ToolResult::error(format!(
                    "No skill named '{}' is available{}",
                    parsed.name, hint
                )))
            }
        }
    }
}

/// Lightweight discovery tool for progressive skill disclosure.
pub struct SkillSearchTool {
    source: Arc<dyn SkillSource>,
}

impl SkillSearchTool {
    pub fn new(source: Arc<dyn SkillSource>) -> Self {
        Self { source }
    }
}

#[derive(Debug, Deserialize)]
struct SkillSearchInput {
    query: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    5
}

#[async_trait]
impl Tool for SkillSearchTool {
    fn name(&self) -> &str {
        "SkillSearch"
    }

    fn description(&self) -> &str {
        "Find relevant skills by keyword without loading their full instructions."
    }

    fn prompt(&self) -> &str {
        "Search skills, then call Skill with the exact selected name."
    }

    fn search_hint(&self) -> Option<&str> {
        Some("find skill workflow instructions")
    }

    fn should_defer(&self) -> bool {
        false
    }

    fn max_result_size_chars(&self) -> usize {
        4_000
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
                    "description": "Capability or workflow keywords."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "default": 5
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
        let parsed: SkillSearchInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(error) => return Ok(ToolResult::error(format!("invalid input: {error}"))),
        };
        let query = parsed.query.trim();
        if query.is_empty() {
            return Ok(ToolResult::error("query must not be empty"));
        }
        let matches = self.source.search_skills(query, parsed.limit.clamp(1, 10));
        if matches.is_empty() {
            return Ok(ToolResult::ok(
                "No matching skills found. Continue without a skill or try broader keywords.",
            ));
        }
        let lines = matches
            .into_iter()
            .map(|entry| {
                let when = entry
                    .when_to_use
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!(" — use when {value}"))
                    .unwrap_or_default();
                format!("- **{}**: {}{}", entry.name, entry.description, when)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult::ok(format!(
            "{lines}\n\nCall `Skill` with the exact selected name to load its instructions."
        )))
    }
}
