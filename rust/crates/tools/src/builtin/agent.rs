//! Agent tool — spawns a subagent for a self-contained subtask. Mirrors the
//! `src/tools/AgentTool/` role (Task/Agent delegation). The actual subagent run
//! is performed by the [`SubagentRunner`](crate::tool::SubagentRunner) supplied
//! by the engine via [`ToolCtx::subagent`]; the subagent gets its own message
//! history and a toolset that excludes `Agent` itself (to prevent unbounded
//! recursion).

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{normalize_optional_profile, SubagentRequest, Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Launches a subagent to handle a self-contained subtask and returns its final answer. Child streaming status, tool activity, permissions, and terminal events are reported under this Agent call.\n\nUse this for:\n- Searches or investigations requiring multiple rounds of tool use\n- Independent, parallelizable work (you may call Agent several times in one turn)\n- Anything that would clutter the main conversation\n\nInput:\n- `prompt`: a complete, self-contained instruction (the subagent does NOT see this conversation — include all needed context, file paths, and the success criterion).\n- `description`: a short (3-5 word) label of what the subagent is doing.\n- `profile`: optional `.nonoclaw/agents/<profile>.md` profile; it may only tighten the parent permissions/tools.\n\nNotes:\n- The subagent runs non-interactively with a restricted toolset (no nested Agent/Coordinator).\n- Prefer specific over vague prompts; state exactly what a successful answer looks like.";

pub struct AgentTool;

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &'static str {
        "Agent"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Launch a subagent for a subtask and return its answer."
    }
    fn search_hint(&self) -> Option<&'static str> {
        Some("delegate subtask subagent background investigation")
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {"type":"string","description":"A short (3-5 word) description of the task"},
                "prompt": {"type":"string","description":"Fully self-contained instruction for the subagent"},
                "profile": {"type":"string","description":"Optional agent profile name from .nonoclaw/agents/<name>.md"}
            },
            "required": ["description", "prompt"]
        })
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        // Subagents may write, so a single Agent call is not concurrency-safe
        // with other writes; the engine runs it sequentially.
        false
    }
    fn max_result_size_chars(&self) -> usize {
        60_000
    }

    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        // Delegating to a subagent is a powerful action; surface it to the user
        // (the subagent itself still goes through the permission gate per tool).
        PermissionDecision::ask("launch a subagent")
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let description = input["description"].as_str().ok_or_else(|| Error::Tool {
            tool: "Agent".into(),
            message: "missing required field `description`".into(),
        })?;
        let prompt = input["prompt"].as_str().ok_or_else(|| Error::Tool {
            tool: "Agent".into(),
            message: "missing required field `prompt`".into(),
        })?;

        let profile =
            normalize_optional_profile(input.get("profile")).map_err(|()| Error::Tool {
                tool: "Agent".into(),
                message: "optional field `profile` must be a string, null, or omitted".into(),
            })?;

        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let Some(runner) = ctx.subagent else {
            return Err(Error::Tool {
                tool: "Agent".into(),
                message: "subagent runner unavailable in this context".into(),
            });
        };

        // Race the subagent against cancellation.
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            r = runner.run_subagent(SubagentRequest {
                prompt: prompt.to_owned(),
                description: description.to_owned(),
                profile,
                parent_tool_use_id: ctx.tool_use_id.to_owned(),
                index: None,
            }) => r,
        }?;

        Ok(ToolResult::ok(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_exposes_optional_profile() {
        let schema = AgentTool.input_schema();
        assert_eq!(schema["properties"]["profile"]["type"], "string");
        assert!(!schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "profile"));
    }
}
