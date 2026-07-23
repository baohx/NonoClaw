//! TodoWrite compatibility adapter over the canonical scoped TaskStore.

use std::sync::Arc;

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result, TaskStatus};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::task_store::TaskStore;
pub use crate::task_store::TodoItem;
use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Use this tool to create and manage a structured task list for your current coding session. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.\n\n## When to Use This Tool\nUse this tool proactively when a task requires 3+ distinct steps, is non-trivial, when the user explicitly requests a todo list, when the user provides multiple tasks, or after receiving new instructions.\n\n## When NOT to Use This Tool\nSkip when there is only a single straightforward task, the task is trivial, or it can be completed in fewer than 3 trivial steps.\n\nProvide the full updated todo list when calling this tool. Mark a task in_progress when starting it (ideally only one at a time) and completed when done.";

/// Compatibility names retained for existing Rust callers.
pub type TodoStatus = TaskStatus;
pub type TodoStore = TaskStore;

pub fn new_store() -> Arc<TodoStore> {
    Arc::new(TaskStore::new())
}

pub struct TodoWriteTool {
    store: Arc<TaskStore>,
}

impl TodoWriteTool {
    pub fn new(store: Arc<TaskStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &'static str {
        "TodoWrite"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Create and manage a structured task list."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type":"string","description":"The task description"},
                            "status": {"type":"string","enum":["pending","in_progress","completed"]},
                            "activeForm": {"type":"string","description":"Present-continuous form for the spinner"}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
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
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let array = input["todos"].as_array().ok_or_else(|| Error::Tool {
            tool: "TodoWrite".into(),
            message: "missing required array field `todos`".into(),
        })?;
        let mut items = Vec::with_capacity(array.len());
        for value in array {
            let content = value["content"]
                .as_str()
                .ok_or_else(|| Error::Tool {
                    tool: "TodoWrite".into(),
                    message: "each todo requires `content`".into(),
                })?
                .to_string();
            // Preserve TodoWrite's historical compatibility fallback. The
            // model-facing schema still accepts only the three canonical values.
            let status = value["status"]
                .as_str()
                .and_then(TaskStatus::parse)
                .unwrap_or(TaskStatus::Pending);
            items.push(TodoItem {
                content,
                status,
                active_form: value["activeForm"].as_str().map(ToOwned::to_owned),
            });
        }

        let total = items.len();
        let done = items
            .iter()
            .filter(|item| item.status == TaskStatus::Completed)
            .count();
        let in_progress = items
            .iter()
            .filter(|item| item.status == TaskStatus::InProgress)
            .count();
        let change = self.store.replace_todos(ctx.task_scope(), items);
        Ok(ToolResult::ok(format!(
            "Todos updated: {done}/{total} completed, {in_progress} in progress"
        ))
        .with_task_change(change))
    }
}

/// Render one agent's current todo scope as a numbered list.
pub fn render(store: &TodoStore, scope: &str) -> String {
    let items = store.todos(scope);
    if items.is_empty() {
        return String::new();
    }
    let mut output = String::from("Task list:\n");
    for (index, item) in items.iter().enumerate() {
        let mark = match item.status {
            TaskStatus::Completed => "[x]",
            TaskStatus::InProgress => "[>]",
            TaskStatus::Pending => "[ ]",
        };
        output.push_str(&format!("  {mark} {}. {}\n", index + 1, item.content));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_store() -> Arc<TaskStore> {
        Arc::new(TaskStore::with_dir(
            std::env::temp_dir().join(format!("nonoclaw-todo-test-{}", uuid::Uuid::new_v4())),
        ))
    }

    #[tokio::test]
    async fn replaces_only_the_callers_scope_and_emits_a_change() {
        // **Validates: Requirements 1.4, 2.3, 2.5**
        let store = fixture_store();
        let tool = TodoWriteTool::new(Arc::clone(&store));
        let options = crate::tool::ToolOptions {
            model: "x".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let ctx = ToolCtx {
            cwd: std::path::Path::new("/tmp"),
            options: &options,
            cancel: &cancel,
            task_scope: Some("parent"),
            subagent: None,
            question: None,
            background_registry: None,
        };
        let result = tool
            .call(
                json!({"todos":[
                    {"content":"a","status":"completed"},
                    {"content":"b","status":"in_progress"},
                    {"content":"c","status":"pending"}
                ]}),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(store.todos("parent").len(), 3);
        assert!(store.todos("child").is_empty());
        assert_eq!(result.task_changes.len(), 1);
        assert_eq!(result.task_changes[0].scope, "parent");
        assert_eq!(result.task_changes[0].tasks.len(), 3);
    }

    #[test]
    fn status_and_render_use_the_shared_domain() {
        let store = fixture_store();
        assert_eq!(
            TaskStatus::parse("in_progress"),
            Some(TaskStatus::InProgress)
        );
        store.replace_todos(
            "agent",
            vec![TodoItem {
                content: "work".into(),
                status: TaskStatus::InProgress,
                active_form: None,
            }],
        );
        assert!(render(&store, "agent").contains("[>] 1. work"));
    }
}
