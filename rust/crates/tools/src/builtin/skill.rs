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

/// Live read-access to skill bodies without coupling the `tools` crate to the
/// engine (which owns `SkillsManager`). Implemented in the engine crate; the
/// tool holds an `Arc<dyn SkillSource>`.
pub trait SkillSource: Send + Sync {
    /// Full, untruncated body for `name`, with `$ARGUMENTS` / `${...}`
    /// substitution applied. `None` if the skill does not exist.
    fn render_skill_body(&self, name: &str, args: &str, session_id: &str) -> Option<String>;

    /// One-line description for a skill (used in error hints). `None` if unknown.
    fn skill_description(&self, name: &str) -> Option<String>;
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
