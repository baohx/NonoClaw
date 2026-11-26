//! Glob tool. Mirrors `src/tools/GlobTool/`. Matches file paths by pattern and
//! returns them sorted by modification time (most recent first).

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "- Fast file pattern matching tool that works with any codebase size\n- Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\"\n- Returns matching file paths sorted by modification time\n- Use this tool when you need to find files by name patterns\n- When you are doing an open ended search that may require multiple rounds of globbing and grepping, use the Agent tool instead";

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "Glob"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Find files by glob pattern."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type":"string","description":"Glob pattern, e.g. \"**/*.rs\""},
                "path": {"type":"string","description":"Directory to search in (default cwd)"}
            },
            "required": ["pattern"]
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
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let pattern = input["pattern"].as_str().ok_or_else(|| Error::Tool {
            tool: "Glob".into(),
            message: "missing required field `pattern`".into(),
        })?;
        let base = input["path"]
            .as_str()
            .map(|p| ctx.environment.resolve_user_path(ctx.cwd, p))
            .unwrap_or_else(|| ctx.environment.effective_cwd(ctx.cwd));

        let mut found = ctx.environment.glob(&base, pattern).await?;

        // Sort most-recent first; files with no mtime sort last.
        found.sort_by_key(|entry| {
            std::cmp::Reverse(entry.modified.unwrap_or(std::time::UNIX_EPOCH))
        });

        if found.is_empty() {
            return Ok(ToolResult::ok("No files matched."));
        }
        let body = found
            .iter()
            .map(|entry| {
                entry
                    .path
                    .strip_prefix(ctx.cwd)
                    .unwrap_or(&entry.path)
                    .display()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult::ok(body))
    }
}
