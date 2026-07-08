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

use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Launches a subagent to handle a self-contained subtask in the background and returns its final answer.\n\nUse this for:\n- Searches or investigations requiring multiple rounds of tool use you don't need to follow step-by-step\n- Independent, parallelizable work (you may call Agent several times in one turn)\n- Anything that would clutter the main conversation\n\nInput:\n- `prompt`: a complete, self-contained instruction (the subagent does NOT see this conversation — include all needed context, file paths, and the success criterion).\n- `description`: a short (3-5 word) label of what the subagent is doing.\n\nNotes:\n- The subagent runs with a restricted toolset (no nested Agent) and reports only its final result, not its steps.\n- Prefer specific over vague prompts; state exactly what a successful answer looks like.";

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
                "prompt": {"type":"string","description":"Fully self-contained instruction for the subagent"}
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
            r = runner.run_subagent(prompt, description) => r,
        }?;

        Ok(ToolResult::ok(result))
    }
}
