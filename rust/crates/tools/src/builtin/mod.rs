//! Built-in core tools. Mirrors `src/tools/`.
//!
//! Each tool is a struct implementing [`crate::tool::Tool`]. The prompts are
//! reproduced (slightly condensed) from each tool's `prompt.ts`.
pub mod agent;
pub mod ask;
pub mod bash;
pub mod coordinator;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod task_tools;
pub mod todo;
pub mod tool_search;
pub mod webfetch;
pub mod websearch;
pub mod write;

pub use agent::AgentTool;
pub use ask::AskUserQuestionTool;
pub use bash::BashTool;
pub use coordinator::CoordinatorTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use todo::{TodoItem, TodoStatus, TodoStore, TodoWriteTool};
pub use tool_search::ToolSearchTool;
pub use webfetch::WebFetchTool;
pub use websearch::WebSearchTool;
pub use write::WriteTool;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::registry::ToolRegistry;

/// Resolve `p` against `cwd` when it is relative.
pub(crate) fn resolve_path(cwd: &Path, p: &str) -> PathBuf {
    let trimmed = p.trim();
    let expanded = expand_tilde(trimmed);
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Expand a leading `~` to the home directory.
pub(crate) fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    } else if p == "~" {
        if let Some(home) = dirs_home() {
            return home;
        }
    }
    PathBuf::from(p)
}

fn dirs_home() -> Option<PathBuf> {
    nonoclaw_core::home_dir()
}

pub(crate) fn nonoclaw_data_dir() -> Option<PathBuf> {
    dirs_home().map(|h| h.join(".nonoclaw"))
}

/// Register all Phase 0 built-in tools. Returns the registry and the shared
/// todo store (so the engine/UI can render the task list).
pub fn register_all() -> (ToolRegistry, Arc<TodoStore>) {
    let todos = todo::new_store();
    let mut reg = ToolRegistry::new();
    reg.register(std::sync::Arc::new(ReadTool));
    reg.register(std::sync::Arc::new(WriteTool));
    reg.register(std::sync::Arc::new(EditTool));
    reg.register(std::sync::Arc::new(BashTool));
    reg.register(std::sync::Arc::new(GrepTool));
    reg.register(std::sync::Arc::new(GlobTool));
    reg.register(std::sync::Arc::new(TodoWriteTool::new(Arc::clone(&todos))));
    reg.register(std::sync::Arc::new(WebFetchTool));
    reg.register(std::sync::Arc::new(WebSearchTool));
    reg.register(std::sync::Arc::new(AgentTool));
    reg.register(std::sync::Arc::new(AskUserQuestionTool));
    reg.register(std::sync::Arc::new(CoordinatorTool));
    let store = task_tools::TaskStore::new();
    reg.register(std::sync::Arc::new(task_tools::TaskCreateTool { store: store.clone() }));
    reg.register(std::sync::Arc::new(task_tools::TaskGetTool { store: store.clone() }));
    reg.register(std::sync::Arc::new(task_tools::TaskListTool { store: store.clone() }));
    reg.register(std::sync::Arc::new(task_tools::TaskUpdateTool { store }));
    (reg, todos)
}
