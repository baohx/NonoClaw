//! Coordinator tool — parallel multi-subagent dispatch. Mirrors the role of
//! `src/coordinator/`: fan out independent subtasks to subagents, gather
//! results. Uses [`SubagentRunner::run_subagents`] for concurrent execution.

use crate::tool::{Tool, ToolCtx, ToolResult};
use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "Dispatch multiple independent subtasks to subagents that run in parallel and return their results aggregated.\n\nUse this for:\n- Searching/reading/investigating multiple independent items at once\n- Anything where subtasks don't depend on each other\n\nInput: a `tasks` array of {description, prompt}, one per subtask.";

pub struct CoordinatorTool;

#[async_trait]
impl Tool for CoordinatorTool {
    fn name(&self) -> &'static str {
        "Coordinator"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Dispatch parallel subtasks to subagents and aggregate."
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"tasks":{"type":"array","items":{"type":"object","properties":{"description":{"type":"string"},"prompt":{"type":"string"}},"required":["description","prompt"]}}},"required":["tasks"]})
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionDecision::ask("dispatch parallel subagents")
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let tasks: Vec<(String, String)> = input["tasks"]
            .as_array()
            .ok_or_else(|| Error::Tool {
                tool: "Coordinator".into(),
                message: "tasks required".into(),
            })?
            .iter()
            .filter_map(|v| {
                let d = v.get("description").and_then(|x| x.as_str())?;
                let p = v.get("prompt").and_then(|x| x.as_str())?;
                Some((p.to_string(), d.to_string()))
            })
            .collect();
        if tasks.is_empty() {
            return Err(Error::Tool {
                tool: "Coordinator".into(),
                message: "no tasks".into(),
            });
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let Some(runner) = ctx.subagent else {
            return Err(Error::Tool {
                tool: "Coordinator".into(),
                message: "subagent runner unavailable".into(),
            });
        };
        let results = tokio::select! {
            biased; _ = cancel.cancelled() => return Err(Error::Cancelled),
            r = runner.run_subagents(&tasks) => r,
        };
        let mut out = String::new();
        for (i, (task, result)) in tasks.iter().zip(results.iter()).enumerate() {
            let body = match result {
                Ok(answer) => answer.as_str(),
                Err(error) => {
                    out.push_str(&format!(
                        "--- Subtask {}: {}\nError: {}\n\n",
                        i + 1,
                        task.1,
                        error
                    ));
                    continue;
                }
            };
            out.push_str(&format!("--- Subtask {}: {}\n{}\n\n", i + 1, task.1, body));
        }
        Ok(ToolResult::ok(out))
    }
}
