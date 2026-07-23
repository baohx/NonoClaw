//! Canonical task storage for TodoWrite and Task*.
//!
//! TodoWrite keeps a scoped, replace-all list. Task* keeps the historical
//! file-backed shared graph. Both use the same status, record projection,
//! serialization helpers, mutation lock, and structured change format.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use nonoclaw_core::{TaskChange, TaskChangeKind, TaskChangeSource, TaskSnapshot, TaskStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TASK_GRAPH_SCOPE: &str = "task_graph";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TodoItem {
    pub content: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

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

#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    pub subject: Option<String>,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub status: Option<TaskStatus>,
    pub add_blocks: Vec<String>,
    pub add_blocked_by: Vec<String>,
}

#[derive(Default)]
struct StoreState {
    todos: HashMap<String, Vec<TodoItem>>,
}

/// One synchronization boundary for scoped todos and the persistent task graph.
pub struct TaskStore {
    dir: PathBuf,
    state: Mutex<StoreState>,
}

impl TaskStore {
    pub fn new() -> Self {
        let dir = nonoclaw_core::nonoclaw_data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tasks");
        Self::with_dir(dir)
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self {
            dir,
            state: Mutex::new(StoreState::default()),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn next_id_locked(&self) -> std::io::Result<String> {
        let highwater = self.dir.join(".highwater");
        let current: u64 = fs::read_to_string(&highwater)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        let next = current + 1;
        fs::write(highwater, next.to_string())?;
        Ok(next.to_string())
    }

    fn save_unlocked(&self, task: &TaskItem) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(task)?;
        fs::write(self.path(&task.id), json)
    }

    fn load_unlocked(&self, id: &str) -> Option<TaskItem> {
        let json = fs::read_to_string(self.path(id)).ok()?;
        serde_json::from_str(&json).ok()
    }

    pub fn save(&self, task: &TaskItem) -> std::io::Result<()> {
        let _guard = self.state.lock().expect("task store poisoned");
        self.save_unlocked(task)
    }

    pub fn load(&self, id: &str) -> Option<TaskItem> {
        let _guard = self.state.lock().expect("task store poisoned");
        self.load_unlocked(id)
    }

    pub fn list(&self) -> Vec<TaskItem> {
        let _guard = self.state.lock().expect("task store poisoned");
        let mut tasks = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = name.strip_suffix(".json") {
                    if let Some(task) = self.load_unlocked(id) {
                        tasks.push(task);
                    }
                }
            }
        }
        tasks.sort_by(|left, right| left.id.cmp(&right.id));
        tasks
    }

    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        let _guard = self.state.lock().expect("task store poisoned");
        fs::remove_file(self.path(id))
    }

    pub fn replace_todos(&self, scope: &str, items: Vec<TodoItem>) -> TaskChange {
        let mut state = self.state.lock().expect("task store poisoned");
        state.todos.insert(scope.to_string(), items.clone());
        TaskChange {
            scope: scope.to_string(),
            source: TaskChangeSource::TodoWrite,
            change: TaskChangeKind::Replaced,
            tasks: items
                .into_iter()
                .enumerate()
                .map(|(index, item)| TaskSnapshot {
                    id: format!("todo:{scope}:{}", index + 1),
                    subject: item.content,
                    status: item.status,
                    active_form: item.active_form,
                    owner: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                })
                .collect(),
        }
    }

    pub fn todos(&self, scope: &str) -> Vec<TodoItem> {
        self.state
            .lock()
            .expect("task store poisoned")
            .todos
            .get(scope)
            .cloned()
            .unwrap_or_default()
    }

    pub fn create_task(
        &self,
        subject: String,
        description: String,
        active_form: Option<String>,
        metadata: HashMap<String, Value>,
    ) -> std::io::Result<(TaskItem, TaskChange)> {
        let _guard = self.state.lock().expect("task store poisoned");
        let task = TaskItem {
            id: self.next_id_locked()?,
            subject,
            description,
            active_form,
            status: TaskStatus::Pending,
            owner: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata,
        };
        self.save_unlocked(&task)?;
        let change = graph_change(TaskChangeSource::TaskCreate, TaskChangeKind::Created, &task);
        Ok((task, change))
    }

    pub fn update_task(
        &self,
        id: &str,
        patch: TaskPatch,
    ) -> std::io::Result<Option<(TaskItem, TaskChange)>> {
        let _guard = self.state.lock().expect("task store poisoned");
        let Some(mut task) = self.load_unlocked(id) else {
            return Ok(None);
        };
        if let Some(subject) = patch.subject {
            task.subject = subject;
        }
        if let Some(description) = patch.description {
            task.description = description;
        }
        if let Some(owner) = patch.owner {
            task.owner = Some(owner);
        }
        if let Some(status) = patch.status {
            task.status = task.status.transition_to(status);
            if status == TaskStatus::InProgress && task.owner.is_none() {
                task.owner = Some("agent".into());
            }
        }
        append_unique(&mut task.blocks, patch.add_blocks);
        append_unique(&mut task.blocked_by, patch.add_blocked_by);
        self.save_unlocked(&task)?;
        let change = graph_change(TaskChangeSource::TaskUpdate, TaskChangeKind::Updated, &task);
        Ok(Some((task, change)))
    }

    pub fn pending_lines(&self) -> Vec<String> {
        let tasks = self.list();
        let completed: HashSet<&str> = tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .map(|task| task.id.as_str())
            .collect();
        tasks
            .iter()
            .filter(|task| task.status != TaskStatus::Completed)
            .map(|task| {
                let blocked = task
                    .blocked_by
                    .iter()
                    .any(|dependency| !completed.contains(dependency.as_str()));
                let prefix = if blocked {
                    "🔒"
                } else {
                    match task.status {
                        TaskStatus::Pending => "○",
                        TaskStatus::InProgress => "●",
                        TaskStatus::Completed => "✓",
                    }
                };
                format!("{prefix} [{}] {}", task.id, task.subject)
            })
            .collect()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new()
    }
}

fn append_unique(target: &mut Vec<String>, additions: Vec<String>) {
    for value in additions {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

fn graph_change(source: TaskChangeSource, change: TaskChangeKind, task: &TaskItem) -> TaskChange {
    TaskChange {
        scope: TASK_GRAPH_SCOPE.into(),
        source,
        change,
        tasks: vec![TaskSnapshot {
            id: task.id.clone(),
            subject: task.subject.clone(),
            status: task.status,
            active_form: task.active_form.clone(),
            owner: task.owner.clone(),
            blocks: task.blocks.clone(),
            blocked_by: task.blocked_by.clone(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_store(name: &str) -> TaskStore {
        let dir = std::env::temp_dir().join(format!(
            "nonoclaw-task-store-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        TaskStore::with_dir(dir)
    }

    #[test]
    fn scoped_todos_cannot_overwrite_another_agent() {
        // **Validates: Requirements 1.4, 2.3**
        let store = fixture_store("scope");
        store.replace_todos(
            "parent",
            vec![TodoItem {
                content: "parent work".into(),
                status: TaskStatus::InProgress,
                active_form: None,
            }],
        );
        store.replace_todos(
            "child",
            vec![TodoItem {
                content: "child work".into(),
                status: TaskStatus::Completed,
                active_form: None,
            }],
        );
        assert_eq!(store.todos("parent")[0].content, "parent work");
        assert_eq!(store.todos("child")[0].content, "child work");
    }

    #[test]
    fn graph_ids_are_monotonic_and_old_json_shape_round_trips() {
        // **Validates: Requirements 1.4, 2.3**
        let store = fixture_store("persistence");
        let (first, _) = store
            .create_task("one".into(), "first".into(), None, HashMap::new())
            .unwrap();
        let (second, _) = store
            .create_task("two".into(), "second".into(), None, HashMap::new())
            .unwrap();
        assert_eq!(first.id, "1");
        assert_eq!(second.id, "2");
        assert_eq!(store.load("1").unwrap().subject, "one");
        let json = fs::read_to_string(store.path("1")).unwrap();
        assert!(!json.contains("task_graph"));
        assert_eq!(serde_json::from_str::<TaskItem>(&json).unwrap().id, "1");
    }

    #[test]
    fn every_status_sequence_preserves_a_declared_state() {
        // **Validates: Requirements 2.3**
        let store = fixture_store("state-machine");
        let (task, _) = store
            .create_task("state".into(), "machine".into(), None, HashMap::new())
            .unwrap();
        let states = [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
        ];
        for mask in 0..81usize {
            let mut value = mask;
            for _ in 0..4 {
                let next = states[value % states.len()];
                value /= states.len();
                let (updated, _) = store
                    .update_task(
                        &task.id,
                        TaskPatch {
                            status: Some(next),
                            ..TaskPatch::default()
                        },
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(updated.status, next);
                assert_eq!(store.load(&task.id).unwrap().status, next);
            }
        }
    }
}
