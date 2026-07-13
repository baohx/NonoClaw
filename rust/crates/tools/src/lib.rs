//! Tool trait, registry, permission engine, and core tool implementations.
//! Mirrors `src/Tool.ts`, `src/tools/`, `src/utils/permissions/`.

pub mod background;
pub mod builtin;
pub mod mcp;
pub mod mcp_server;
pub mod permissions;
pub mod registry;
pub mod tool;

pub use builtin::register_all;
pub use mcp::{
    load_config as load_mcp_config, register as register_mcp, McpClient, McpServerConfig,
};
pub use permissions::{pattern_matches, wildcard_match, PermissionGate};
pub use registry::ToolRegistry;
pub use tool::{
    allow_if_read_only, matches_name, QuestionRequest, QuestionResolver, SubagentRunner, Tool,
    ToolDefinition, ToolOptions, ToolResult,
};

pub use background::BackgroundTaskRegistry;
pub use builtin::{TodoItem, TodoStatus, TodoStore};
