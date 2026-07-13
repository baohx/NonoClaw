//! TodoWrite tool. Mirrors `src/tools/TodoWriteTool/`. Maintains a structured
//! task list in a process-shared store so the engine/UI can render progress.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Use this tool to create and manage a structured task list for your current coding session. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.\n\n## When to Use This Tool\nUse this tool proactively when a task requires 3+ distinct steps, is non-trivial, when the user explicitly requests a todo list, when the user provides multiple tasks, or after receiving new instructions.\n\n## When NOT to Use This Tool\nSkip when there is only a single straightforward task, the task is trivial, or it can be completed in fewer than 3 trivial steps.\n\nProvide the full updated todo list when calling this tool. Mark a task in_progress when starting it (ideally only one at a time) and completed when done.";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "in_progress" => TodoStatus::InProgress,
            "completed" => TodoStatus::Completed,
            _ => TodoStatus::Pending,
        }
    }
    #[allow(dead_code)]
    fn label(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

/// Shared todo list. `Arc<TodoStore>` is held by the tool and the engine.
pub type TodoStore = Mutex<Vec<TodoItem>>;

pub fn new_store() -> Arc<TodoStore> {
    Arc::new(Mutex::new(Vec::new()))
}

pub struct TodoWriteTool {
    store: Arc<TodoStore>,
}

impl TodoWriteTool {
    pub fn new(store: Arc<TodoStore>) -> Self {
        TodoWriteTool { store }
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
        // Updating the in-memory todo list is not destructive to the user's
        // filesystem; auto-allow.
        PermissionResult::allow()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let arr = input["todos"].as_array().ok_or_else(|| Error::Tool {
            tool: "TodoWrite".into(),
            message: "missing required array field `todos`".into(),
        })?;
        let mut items: Vec<TodoItem> = Vec::with_capacity(arr.len());
        for v in arr {
            let content = v["content"]
                .as_str()
                .ok_or_else(|| Error::Tool {
                    tool: "TodoWrite".into(),
                    message: "each todo requires `content`".into(),
                })?
                .to_string();
            let status = TodoStatus::from_str(v["status"].as_str().unwrap_or("pending"));
            let active_form = v["activeForm"].as_str().map(|s| s.to_string());
            items.push(TodoItem {
                content,
                status,
                active_form,
            });
        }

        let summary = {
            let mut store = self.store.lock().expect("todo store poisoned");
            let total = items.len();
            let done = items
                .iter()
                .filter(|t| t.status == TodoStatus::Completed)
                .count();
            let in_prog = items
                .iter()
                .filter(|t| t.status == TodoStatus::InProgress)
                .count();
            *store = items;
            format!("Todos updated: {done}/{total} completed, {in_prog} in progress")
        };
        Ok(ToolResult::ok(summary))
    }
}

/// Render the current store as a numbered list (used by the engine for display).
pub fn render(store: &TodoStore) -> String {
    let store = store.lock().expect("todo store poisoned");
    if store.is_empty() {
        return String::new();
    }
    let mut out = String::from("Task list:\n");
    for (i, t) in store.iter().enumerate() {
        let mark = match t.status {
            TodoStatus::Completed => "[x]",
            TodoStatus::InProgress => "[>]",
            TodoStatus::Pending => "[ ]",
        };
        out.push_str(&format!("  {} {}. {}\n", mark, i + 1, t.content));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replaces_store_contents() {
        let store = new_store();
        let tool = TodoWriteTool::new(Arc::clone(&store));
        let opts = crate::tool::ToolOptions {
            model: "x".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let cwd = std::path::Path::new("/tmp");
        let ctx = ToolCtx {
            cwd,
            options: &opts,
            cancel: &cancel,
            subagent: None,
            question: None,
            background_registry: None,
        };
        let input = json!({"todos":[
            {"content":"a","status":"completed"},
            {"content":"b","status":"in_progress"},
            {"content":"c","status":"pending"}
        ]});
        tool.call(input, &ctx, CancellationToken::new())
            .await
            .unwrap();
        let s = store.lock().unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].status, TodoStatus::Completed);
        assert_eq!(s[1].status, TodoStatus::InProgress);
        assert_eq!(s[2].status, TodoStatus::Pending);
    }

    #[test]
    fn status_round_trip() {
        assert_eq!(TodoStatus::from_str("in_progress"), TodoStatus::InProgress);
        assert_eq!(TodoStatus::InProgress.label(), "in_progress");
    }
}
