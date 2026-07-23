//! Built-in core tools. Mirrors `src/tools/`.
//!
//! Each tool is a struct implementing [`crate::tool::Tool`]. The prompts are
//! reproduced (slightly condensed) from each tool's `prompt.ts`.
pub mod agent;
pub mod ask;
pub mod background_tasks;
pub mod bash;
pub mod coordinator;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod lsp;
pub mod memory;
pub mod read;
pub mod task_tools;
pub mod todo;
pub mod tool_search;
pub mod webfetch;
pub mod websearch;
pub mod write;

pub use agent::AgentTool;
pub use ask::AskUserQuestionTool;
pub use background_tasks::{TaskOutputTool, TaskStopTool};
pub use bash::BashTool;
pub use coordinator::CoordinatorTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use lsp::LspTool;
pub use memory::MemoryTool;
pub use read::ReadTool;
pub use todo::{TodoStatus, TodoStore, TodoWriteTool};
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

/// Register the complete core tool set. Returns the registry and the shared
/// task store adapter used by both TodoWrite and Task* tools.
pub fn register_all() -> (ToolRegistry, Arc<TodoStore>) {
    let todos = todo::new_store();
    let mut reg = ToolRegistry::new();
    reg.register(std::sync::Arc::new(ReadTool));
    reg.register(std::sync::Arc::new(WriteTool));
    reg.register(std::sync::Arc::new(EditTool));
    reg.register(std::sync::Arc::new(BashTool));
    reg.register(std::sync::Arc::new(TaskOutputTool));
    reg.register(std::sync::Arc::new(TaskStopTool));
    reg.register(std::sync::Arc::new(GrepTool));
    reg.register(std::sync::Arc::new(GlobTool));
    reg.register(std::sync::Arc::new(TodoWriteTool::new(Arc::clone(&todos))));
    reg.register(std::sync::Arc::new(WebFetchTool));
    reg.register(std::sync::Arc::new(WebSearchTool));
    reg.register(std::sync::Arc::new(MemoryTool));
    reg.register(std::sync::Arc::new(LspTool::new()));
    reg.register(std::sync::Arc::new(AgentTool));
    reg.register(std::sync::Arc::new(AskUserQuestionTool));
    reg.register(std::sync::Arc::new(CoordinatorTool));
    reg.register(std::sync::Arc::new(task_tools::TaskCreateTool {
        store: Arc::clone(&todos),
    }));
    reg.register(std::sync::Arc::new(task_tools::TaskGetTool {
        store: Arc::clone(&todos),
    }));
    reg.register(std::sync::Arc::new(task_tools::TaskListTool {
        store: Arc::clone(&todos),
    }));
    reg.register(std::sync::Arc::new(task_tools::TaskUpdateTool {
        store: Arc::clone(&todos),
    }));
    (reg, todos)
}

#[cfg(test)]
mod characterization_tests {
    use super::*;
    use serde_json::json;

    /// Characterization contract for the normal CLI/Web registry: the 18 core
    /// tools plus ToolSearch, which main registers after MCP discovery.
    /// Feature Preservation Matrix: sections 3.1 and 10; Requirements 1.4, 11.7.
    #[test]
    fn tool_registration_names_and_schemas_match_snapshot() {
        let (mut registry, _) = register_all();
        let search_entries = registry.search_entries();
        registry.register(Arc::new(ToolSearchTool::new(search_entries)));

        let tools: Vec<_> = registry
            .definitions(None)
            .into_iter()
            .map(|definition| {
                json!({
                    "name": definition.name,
                    "input_schema": definition.input_schema,
                })
            })
            .collect();
        let actual = format!(
            "{}\n",
            serde_json::to_string_pretty(&tools).expect("serialize tool contract")
        );

        let snapshot_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/snapshots/builtin_tool_contract.json");
        if std::env::var_os("UPDATE_TOOL_SNAPSHOT").is_some() {
            std::fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
            std::fs::write(&snapshot_path, &actual).unwrap();
        }
        let expected = std::fs::read_to_string(&snapshot_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", snapshot_path.display()));
        assert_eq!(
            actual, expected,
            "tool names or model-facing schemas changed"
        );
    }
}
