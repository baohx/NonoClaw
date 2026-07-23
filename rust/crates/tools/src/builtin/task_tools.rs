//! TaskCreate/Get/List/Update compatibility adapters over TaskStore.

use std::sync::Arc;

use async_trait::async_trait;
use nonoclaw_core::{PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::task_store::TaskPatch;
pub use crate::task_store::{TaskItem, TaskStore};
use crate::tool::{Tool, ToolCtx, ToolResult};
pub use nonoclaw_core::TaskStatus;

// ── TaskCreateTool ──────────────────────────────────────────────────────────

pub struct TaskCreateTool {
    pub store: Arc<TaskStore>,
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "TaskCreate"
    }
    fn description(&self) -> &str {
        "Create a task in the task list."
    }
    fn prompt(&self) -> &str {
        "Create a task: provide subject (title), description (details), optional activeForm (present continuous form for progress display)."
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn aliases(&self) -> &[&str] {
        &[]
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn search_hint(&self) -> Option<&str> {
        Some("create task todo item")
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": {"type": "string", "description": "Task title"},
                "description": {"type": "string", "description": "Task details"},
                "activeForm": {"type": "string", "description": "Present continuous (e.g. 'Running tests')"},
                "metadata": {"type": "object"}
            },
            "required": ["subject", "description"]
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
        let subject = input["subject"].as_str().unwrap_or("Untitled").to_string();
        let description = input["description"].as_str().unwrap_or("").to_string();
        let active_form = input["activeForm"].as_str().map(ToOwned::to_owned);
        let metadata = input["metadata"]
            .as_object()
            .map(|values| {
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let (task, change) = self
            .store
            .create_task(subject, description, active_form, metadata)
            .map_err(|error| nonoclaw_core::Error::Tool {
                tool: "TaskCreate".into(),
                message: format!("save: {error}"),
            })?;
        Ok(ToolResult::ok(format!("Task {} created.", task.id)).with_task_change(change))
    }
}

// ── TaskGetTool ─────────────────────────────────────────────────────────────

pub struct TaskGetTool {
    pub store: Arc<TaskStore>,
}

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        "TaskGet"
    }
    fn description(&self) -> &str {
        "Get a task by ID."
    }
    fn prompt(&self) -> &str {
        "Get a task: provide taskId."
    }
    fn should_defer(&self) -> bool {
        true
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
    fn search_hint(&self) -> Option<&str> {
        Some("get task details by id")
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"taskId": {"type": "string"}},
            "required": ["taskId"]
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
        let id = input["taskId"].as_str().unwrap_or("");
        match self.store.load(id) {
            Some(task) => Ok(ToolResult::ok(
                serde_json::to_string_pretty(&json!({
                    "id": task.id,
                    "subject": task.subject,
                    "description": task.description,
                    "status": task.status.to_string(),
                    "blocks": task.blocks,
                    "blockedBy": task.blocked_by
                }))
                .unwrap_or_default(),
            )),
            None => Ok(ToolResult::ok(format!("Task {id} not found."))),
        }
    }
}

// ── TaskListTool ────────────────────────────────────────────────────────────

pub struct TaskListTool {
    pub store: Arc<TaskStore>,
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }
    fn description(&self) -> &str {
        "List all tasks."
    }
    fn prompt(&self) -> &str {
        "List all tasks with status."
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
    fn search_hint(&self) -> Option<&str> {
        Some("list all tasks status")
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionResult::allow()
    }

    async fn call(
        &self,
        _input: Value,
        _ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let lines = self.store.pending_lines();
        if lines.is_empty() {
            Ok(ToolResult::ok("No pending tasks."))
        } else {
            Ok(ToolResult::ok(lines.join("\n")))
        }
    }
}

// ── TaskUpdateTool ──────────────────────────────────────────────────────────

pub struct TaskUpdateTool {
    pub store: Arc<TaskStore>,
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "TaskUpdate"
    }
    fn description(&self) -> &str {
        "Update task status, fields, or dependencies."
    }
    fn prompt(&self) -> &str {
        "Update a task: provide taskId and fields to change (status, subject, description, addBlocks, addBlockedBy, owner). Status: pending/in_progress/completed."
    }
    fn should_defer(&self) -> bool {
        false
    }
    fn aliases(&self) -> &[&str] {
        &[]
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn search_hint(&self) -> Option<&str> {
        Some("update task status dependency")
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "taskId": {"type": "string"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                "subject": {"type": "string"},
                "description": {"type": "string"},
                "owner": {"type": "string"},
                "addBlocks": {"type": "array", "items": {"type": "string"}},
                "addBlockedBy": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["taskId"]
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
        let id = input["taskId"].as_str().unwrap_or("");
        let patch = TaskPatch {
            subject: input["subject"].as_str().map(ToOwned::to_owned),
            description: input["description"].as_str().map(ToOwned::to_owned),
            owner: input["owner"].as_str().map(ToOwned::to_owned),
            status: input["status"].as_str().and_then(TaskStatus::parse),
            add_blocks: string_array(&input["addBlocks"]),
            add_blocked_by: string_array(&input["addBlockedBy"]),
        };
        let updated =
            self.store
                .update_task(id, patch)
                .map_err(|error| nonoclaw_core::Error::Tool {
                    tool: "TaskUpdate".into(),
                    message: format!("save: {error}"),
                })?;
        let Some((task, change)) = updated else {
            return Ok(ToolResult::error(format!("Task {id} not found.")));
        };
        Ok(
            ToolResult::ok(format!("Task {id} updated. Status: {}", task.status))
                .with_task_change(change),
        )
    }
}

fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolCtx, ToolOptions};

    fn fixture() -> (Arc<TaskStore>, ToolOptions, CancellationToken) {
        let store = Arc::new(TaskStore::with_dir(
            std::env::temp_dir().join(format!("nonoclaw-task-tools-{}", uuid::Uuid::new_v4())),
        ));
        let options = ToolOptions {
            model: "test".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        (store, options, CancellationToken::new())
    }

    #[tokio::test]
    async fn legacy_outputs_and_structured_changes_are_both_preserved() {
        // **Validates: Requirements 1.4, 2.3, 2.5**
        let (store, options, cancel) = fixture();
        let ctx = ToolCtx {
            cwd: std::path::Path::new("/tmp"),
            options: &options,
            cancel: &cancel,
            task_scope: Some("coordinator"),
            subagent: None,
            question: None,
            background_registry: None,
        };
        let created = TaskCreateTool {
            store: Arc::clone(&store),
        }
        .call(
            json!({"subject":"work","description":"details"}),
            &ctx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(created.data, "Task 1 created.");
        assert_eq!(created.task_changes[0].scope, "task_graph");

        let updated = TaskUpdateTool {
            store: Arc::clone(&store),
        }
        .call(
            json!({"taskId":"1","status":"in_progress","addBlockedBy":["2","2"]}),
            &ctx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(updated.data, "Task 1 updated. Status: in_progress");
        let task = store.load("1").unwrap();
        assert_eq!(task.owner.as_deref(), Some("agent"));
        assert_eq!(task.blocked_by, vec!["2"]);
        assert_eq!(
            updated.task_changes[0].tasks[0].status,
            TaskStatus::InProgress
        );
    }
}
