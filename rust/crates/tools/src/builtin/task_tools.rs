//! Task management tools (TaskCreate/Get/List/Update). Mirrors CC's
//! TaskCreateTool/Get/List/Update with file-based persistence, dependency
//! graph, and owner assignment.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use nonoclaw_core::{PermissionDecision, PermissionResult, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskItem {
    pub id: String,
    pub subject: String,
    pub description: String,
    #[serde(default)]
    pub active_form: Option<String>,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Pending => write!(f, "pending"),
            TaskStatus::InProgress => write!(f, "in_progress"),
            TaskStatus::Completed => write!(f, "completed"),
        }
    }
}

/// File-based task store. Each task is a JSON file in `~/.nonoclaw/tasks/`.
#[derive(Clone)]
pub struct TaskStore {
    dir: PathBuf,
}

impl TaskStore {
    pub fn new() -> Self {
        let dir = crate::builtin::nonoclaw_data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tasks");
        let _ = fs::create_dir_all(&dir);
        TaskStore { dir }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn next_id(&self) -> String {
        // Simple monotonic counter via file count + highwater mark.
        let highwater = self.dir.join(".highwater");
        let current: u64 = fs::read_to_string(&highwater)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let next = current + 1;
        let _ = fs::write(&highwater, next.to_string());
        format!("{next}")
    }

    pub fn save(&self, task: &TaskItem) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(task)?;
        fs::write(self.path(&task.id), json)
    }

    pub fn load(&self, id: &str) -> Option<TaskItem> {
        let json = fs::read_to_string(self.path(id)).ok()?;
        serde_json::from_str(&json).ok()
    }

    pub fn list(&self) -> Vec<TaskItem> {
        let mut tasks = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".json") && name != ".highwater" {
                    if let Some(task) = self.load(name.trim_end_matches(".json")) {
                        tasks.push(task);
                    }
                }
            }
        }
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        tasks
    }

    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        fs::remove_file(self.path(id))
    }
}

fn nonoclaw_data_dir() -> Option<PathBuf> {
    nonoclaw_core::home_dir().map(|h| h.join(".nonoclaw"))
}

// ── TaskCreateTool ──────────────────────────────────────────────────────────

pub struct TaskCreateTool {
    pub store: TaskStore,
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str { "TaskCreate" }
    fn description(&self) -> &str { "Create a task in the task list." }
    fn prompt(&self) -> &str {
        "Create a task: provide subject (title), description (details), optional activeForm (present continuous form for progress display)."
    }
    fn should_defer(&self) -> bool { true }
    fn aliases(&self) -> &[&str] { &[] }
    fn is_read_only(&self, _: &Value) -> bool { false }
    fn is_concurrency_safe(&self, _: &Value) -> bool { false }
    fn search_hint(&self) -> Option<&str> { Some("create task todo item") }

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

    async fn call(&self, input: Value, _ctx: &ToolCtx<'_>, _cancel: CancellationToken) -> Result<ToolResult> {
        let subj = input["subject"].as_str().unwrap_or("Untitled").to_string();
        let desc = input["description"].as_str().unwrap_or("").to_string();
        let active = input["activeForm"].as_str().map(|s| s.to_string());
        let id = self.store.next_id();
        let task = TaskItem {
            id: id.clone(),
            subject: subj,
            description: desc,
            active_form: active,
            status: TaskStatus::Pending,
            owner: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata: HashMap::new(),
        };
        self.store.save(&task).map_err(|e| nonoclaw_core::Error::Tool { tool: "TaskCreate".into(), message: format!("save: {e}") })?;
        Ok(ToolResult::ok(format!("Task {id} created.")))
    }
}

// ── TaskGetTool ─────────────────────────────────────────────────────────────

pub struct TaskGetTool {
    pub store: TaskStore,
}

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str { "TaskGet" }
    fn description(&self) -> &str { "Get a task by ID." }
    fn prompt(&self) -> &str { "Get a task: provide taskId." }
    fn should_defer(&self) -> bool { true }
    fn aliases(&self) -> &[&str] { &[] }
    fn is_read_only(&self, _: &Value) -> bool { true }
    fn is_concurrency_safe(&self, _: &Value) -> bool { true }
    fn search_hint(&self) -> Option<&str> { Some("get task details by id") }

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

    async fn call(&self, input: Value, _ctx: &ToolCtx<'_>, _cancel: CancellationToken) -> Result<ToolResult> {
        let id = input["taskId"].as_str().unwrap_or("");
        match self.store.load(id) {
            Some(task) => Ok(ToolResult::ok(serde_json::to_string_pretty(&json!({
                "id": task.id,
                "subject": task.subject,
                "description": task.description,
                "status": task.status.to_string(),
                "blocks": task.blocks,
                "blockedBy": task.blocked_by
            })).unwrap_or_default())),
            None => Ok(ToolResult::ok(format!("Task {id} not found."))),
        }
    }
}

// ── TaskListTool ────────────────────────────────────────────────────────────

pub struct TaskListTool {
    pub store: TaskStore,
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str { "TaskList" }
    fn description(&self) -> &str { "List all tasks." }
    fn prompt(&self) -> &str { "List all tasks with status." }
    fn should_defer(&self) -> bool { false }
    fn aliases(&self) -> &[&str] { &[] }
    fn is_read_only(&self, _: &Value) -> bool { true }
    fn is_concurrency_safe(&self, _: &Value) -> bool { true }
    fn search_hint(&self) -> Option<&str> { Some("list all tasks status") }

    fn input_schema(&self) -> Value { json!({"type": "object", "properties": {}}) }

    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionResult::allow()
    }

    async fn call(&self, _input: Value, _ctx: &ToolCtx<'_>, _cancel: CancellationToken) -> Result<ToolResult> {
        let tasks = self.store.list();
        let completed: HashSet<&str> = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|t| t.id.as_str())
            .collect();
        let lines: Vec<String> = tasks
            .iter()
            .filter(|t| t.status != TaskStatus::Completed)
            .map(|t| {
                let blocked = t.blocked_by.iter().any(|b| !completed.contains(b.as_str()));
                let prefix = if blocked { "🔒" } else { match t.status { TaskStatus::Pending => "○", TaskStatus::InProgress => "●", TaskStatus::Completed => "✓" } };
                format!("{prefix} [{id}] {subj}", id = t.id, subj = t.subject)
            })
            .collect();
        if lines.is_empty() {
            Ok(ToolResult::ok("No pending tasks."))
        } else {
            Ok(ToolResult::ok(lines.join("\n")))
        }
    }
}

// ── TaskUpdateTool ──────────────────────────────────────────────────────────

pub struct TaskUpdateTool {
    pub store: TaskStore,
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str { "TaskUpdate" }
    fn description(&self) -> &str { "Update task status, fields, or dependencies." }
    fn prompt(&self) -> &str {
        "Update a task: provide taskId and fields to change (status, subject, description, addBlocks, addBlockedBy, owner). Status: pending/in_progress/completed."
    }
    fn should_defer(&self) -> bool { false }
    fn aliases(&self) -> &[&str] { &[] }
    fn is_read_only(&self, _: &Value) -> bool { false }
    fn is_concurrency_safe(&self, _: &Value) -> bool { false }
    fn search_hint(&self) -> Option<&str> { Some("update task status dependency") }

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

    async fn call(&self, input: Value, _ctx: &ToolCtx<'_>, _cancel: CancellationToken) -> Result<ToolResult> {
        let id = input["taskId"].as_str().unwrap_or("");
        let mut task = match self.store.load(id) {
            Some(t) => t,
            None => return Ok(ToolResult::error(format!("Task {id} not found."))),
        };

        if let Some(s) = input["subject"].as_str() { task.subject = s.to_string(); }
        if let Some(d) = input["description"].as_str() { task.description = d.to_string(); }
        if let Some(o) = input["owner"].as_str() { task.owner = Some(o.to_string()); }
        if let Some(status_str) = input["status"].as_str() {
            task.status = match status_str {
                "pending" => TaskStatus::Pending,
                "in_progress" => { if task.owner.is_none() { task.owner = Some("agent".into()); } TaskStatus::InProgress }
                "completed" => TaskStatus::Completed,
                _ => task.status,
            };
        }
        if let Some(blocks) = input["addBlocks"].as_array() {
            for b in blocks { if let Some(s) = b.as_str() { if !task.blocks.contains(&s.to_string()) { task.blocks.push(s.to_string()); } } }
        }
        if let Some(blocked) = input["addBlockedBy"].as_array() {
            for b in blocked { if let Some(s) = b.as_str() { if !task.blocked_by.contains(&s.to_string()) { task.blocked_by.push(s.to_string()); } } }
        }

        self.store.save(&task).map_err(|e| nonoclaw_core::Error::Tool { tool: "TaskUpdate".into(), message: format!("save: {e}") })?;
        Ok(ToolResult::ok(format!("Task {id} updated. Status: {}", task.status)))
    }
}
