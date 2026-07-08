//! The `Tool` abstraction. Mirrors the `Tool<Input, Output, Progress>`
//! interface in `src/Tool.ts:362` and the `ToolUseContext` at `src/Tool.ts:158`.
//!
//! Each built-in tool is a struct implementing [`Tool`]; the TS `buildTool`
//! builder is unnecessary in Rust. The model-facing description (`prompt.ts` in
//! the TS tree) is returned by [`Tool::prompt`]; the wire schema comes from
//! [`Tool::input_schema`].

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use async_trait::async_trait;
use nonoclaw_core::{
    Error, Message, PermissionDecision, PermissionMode, PermissionResult, Result, ValidationResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Spawns a subagent (a recursive engine run with its own message history and a
/// restricted toolset) and returns its final text answer. Provided by the
/// engine; tools that need it (Agent) read it from [`ToolCtx::subagent`].
///
/// The returned future borrows `self` for its lifetime (the engine owns the
/// runner for the duration of the run).
pub trait SubagentRunner: Send + Sync {
    fn run_subagent<'a>(
        &'a self,
        prompt: &'a str,
        description: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

    /// Run multiple subagents concurrently; default falls back to sequential.
    fn run_subagents<'a>(
        &'a self,
        tasks: &'a [(String, String)],
    ) -> Pin<Box<dyn Future<Output = Vec<Result<String>>> + Send + 'a>> {
        Box::pin(async move {
            let mut out = Vec::new();
            for (prompt, desc) in tasks {
                out.push(self.run_subagent(prompt, desc).await);
            }
            out
        })
    }
}

/// A question put to the user by an interactive tool (AskUserQuestion).
#[derive(Debug, Clone)]
pub struct QuestionRequest {
    pub prompt: String,
    pub options: Vec<String>,
}

/// Asks the user a multiple-choice question and returns the chosen option text
/// (or `None` if dismissed). Provided by the TUI; headless runs have none.
pub trait QuestionResolver: Send + Sync {
    fn ask<'a>(
        &'a self,
        req: QuestionRequest,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;
}

/// Session-wide options shared by every tool invocation. Mirrors the subset of
/// `ToolUseContext.options` needed for Phase 0.
#[derive(Debug, Clone)]
pub struct ToolOptions {
    pub model: String,
    pub permission_mode: PermissionMode,
    /// `true` for `--print` / SDK mode; tools that need interaction must degrade.
    pub is_non_interactive: bool,
    pub max_budget_usd: Option<f64>,
}

/// Context passed into each tool method. Mirrors `ToolUseContext`.
pub struct ToolCtx<'a> {
    pub cwd: &'a Path,
    pub options: &'a ToolOptions,
    pub cancel: &'a CancellationToken,
    /// Subagent runner (set by the engine). `None` in tests / when subagents
    /// are unavailable; the Agent tool errors if it's missing.
    pub subagent: Option<&'a dyn SubagentRunner>,
    /// Interactive question resolver (AskUserQuestion). `None` when headless.
    pub question: Option<&'a dyn QuestionResolver>,
}

impl<'a> ToolCtx<'a> {
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// What a tool returns from [`Tool::call`]. Mirrors `ToolResult` (`src/Tool.ts:321`).
/// `data` is the primary string result sent back to the model; `new_messages`
/// lets a tool inject extra transcript messages (rare in Phase 0).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolResult {
    pub data: String,
    #[serde(default)]
    pub new_messages: Vec<Message>,
}

impl ToolResult {
    pub fn ok<S: Into<String>>(data: S) -> Self {
        ToolResult {
            data: data.into(),
            new_messages: Vec::new(),
        }
    }
    pub fn error<S: Into<String>>(message: S) -> Self {
        ToolResult {
            data: message.into(),
            new_messages: Vec::new(),
        }
    }
}

/// A tool definition in the shape sent to the API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// The trait every tool implements. Method defaults mirror the TS interface's
/// defaults; `description`/`prompt`/`input_schema`/`call`/`check_permissions`
/// are required.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Primary tool name (e.g. `"Read"`). Borrows from `self` so dynamically
    /// named tools (MCP) can return a composed name.
    fn name(&self) -> &str;

    /// Backwards-compatible aliases. Looked up in addition to [`name`].
    fn aliases(&self) -> &[&str] {
        &[]
    }

    /// Short capability phrase for keyword search. 3–10 words.
    fn search_hint(&self) -> Option<&str> {
        None
    }

    /// Model-facing prompt text (the `prompt.ts` content).
    fn prompt(&self) -> &str;

    /// One-line human-facing description (shown in tool listings).
    fn description(&self) -> &str;

    /// JSON Schema describing the input object. Serialized into the API request.
    fn input_schema(&self) -> Value;

    /// `true` if the operation does not mutate state. Read-only tools may be
    /// auto-approved more liberally.
    fn is_read_only(&self, input: &Value) -> bool;

    /// `true` if concurrent execution with other tools is safe.
    fn is_concurrency_safe(&self, input: &Value) -> bool;

    /// `true` for irreversible operations (delete, overwrite, send). Default false.
    fn is_destructive(&self, _input: &Value) -> bool {
        false
    }

    fn is_enabled(&self) -> bool {
        true
    }

    /// Deferred tools require ToolSearch before use. Always false in Phase 0.
    fn should_defer(&self) -> bool {
        false
    }

    /// Max result size before the engine persists the result to disk and gives
    /// the model a preview + path. Default 30k chars.
    fn max_result_size_chars(&self) -> usize {
        30_000
    }

    /// Pre-permission validation. Default passes.
    async fn validate_input(&self, _input: &Value, _ctx: &ToolCtx<'_>) -> ValidationResult {
        ValidationResult::ok()
    }

    /// Tool-specific permission logic. General (mode/rule) logic lives in the
    /// permission engine; this composes with it.
    async fn check_permissions(&self, input: &Value, ctx: &ToolCtx<'_>) -> PermissionResult;

    /// Execute the tool. Returns the result data or an error.
    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult>;

    /// Convenience: build the API tool definition from this tool.
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// Match a query name against a tool's name + aliases. Mirrors `toolMatchesName`.
pub fn matches_name(name: &str, aliases: &[&str], query: &str) -> bool {
    name == query || aliases.contains(&query)
}

/// Permission decision helper used by tools: read-only tools allow by default.
pub fn allow_if_read_only(is_read_only: bool) -> PermissionResult {
    if is_read_only {
        PermissionDecision::allow()
    } else {
        PermissionDecision::ask("write operation requires permission")
    }
}

// Re-export core error alias for tool implementers.
pub type ToolError = Error;
