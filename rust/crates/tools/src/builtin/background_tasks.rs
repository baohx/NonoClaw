//! Query and stop tools for shell tasks owned by `BackgroundTaskManager`.

use std::time::Duration;

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "TaskOutput"
    }

    fn prompt(&self) -> &str {
        "Read the current status and captured output of a background Bash task. Set block=true to wait up to timeout_ms for completion."
    }

    fn description(&self) -> &str {
        "Read status and output from a background Bash task."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Background task ID returned by Bash"},
                "block": {"type": "boolean", "description": "Wait for the task to finish before returning"},
                "timeout_ms": {"type": "integer", "description": "Maximum wait when block=true (default 30000, max 120000)"}
            },
            "required": ["task_id"]
        })
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
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let task_id = input["task_id"].as_str().ok_or_else(|| Error::Tool {
            tool: self.name().into(),
            message: "missing required string field `task_id`".into(),
        })?;
        let manager = ctx
            .background_registry
            .as_ref()
            .map(|registry| registry.lock().unwrap().clone())
            .ok_or_else(|| Error::Tool {
                tool: self.name().into(),
                message: "background task manager unavailable".into(),
            })?;

        let mut task = manager.get_task(task_id).ok_or_else(|| Error::Tool {
            tool: self.name().into(),
            message: format!("background task `{task_id}` not found"),
        })?;
        if input["block"].as_bool().unwrap_or(false) && !task.status.is_terminal() {
            let timeout_ms = input["timeout_ms"].as_u64().unwrap_or(30_000).min(120_000);
            task = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                task = manager.wait_for_terminal(task_id, Duration::from_millis(timeout_ms)) => {
                    task.ok_or_else(|| Error::Tool {
                        tool: self.name().into(),
                        message: format!("background task `{task_id}` disappeared"),
                    })?
                }
            };
        }
        let output = manager.read_output(task_id).unwrap_or_default();
        Ok(ToolResult::ok(format!(
            "Task ID: {}\nStatus: {:?}\nExit code: {}\nOutput:\n{}",
            task.id,
            task.status,
            task.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".into()),
            output
        )))
    }
}

pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn prompt(&self) -> &str {
        "Stop a running background Bash task by its task_id. The process and its descendants are terminated and reaped by the background task manager."
    }

    fn description(&self) -> &str {
        "Stop a background Bash task."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Background task ID returned by Bash"}
            },
            "required": ["task_id"]
        })
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionDecision::ask("stop a background shell task")
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let task_id = input["task_id"].as_str().ok_or_else(|| Error::Tool {
            tool: self.name().into(),
            message: "missing required string field `task_id`".into(),
        })?;
        let stopped = ctx
            .background_registry
            .as_ref()
            .map(|registry| registry.lock().unwrap().stop(task_id))
            .ok_or_else(|| Error::Tool {
                tool: self.name().into(),
                message: "background task manager unavailable".into(),
            })?;
        if stopped {
            Ok(ToolResult::ok(format!(
                "Stop requested for background task {task_id}."
            )))
        } else {
            Ok(ToolResult::error(format!(
                "Background task {task_id} was not found or is already finished."
            )))
        }
    }
}
