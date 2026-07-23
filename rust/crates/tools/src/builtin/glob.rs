//! Glob tool. Mirrors `src/tools/GlobTool/`. Matches file paths by pattern and
//! returns them sorted by modification time (most recent first).

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::builtin::resolve_path;
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
            .map(|p| resolve_path(ctx.cwd, p))
            .unwrap_or_else(|| ctx.cwd.to_path_buf());

        let full_pattern = join_pattern(&base, pattern);
        let entries = glob::glob(&full_pattern).map_err(|e| Error::Tool {
            tool: "Glob".into(),
            message: format!("invalid pattern `{pattern}`: {e}"),
        })?;

        // Collect existing paths with mtimes, then sort most-recent first.
        let mut found: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
        for entry in entries {
            match entry {
                Ok(p) => {
                    if let Ok(meta) = std::fs::metadata(&p) {
                        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                        found.push((mtime, p));
                    }
                }
                Err(_) => continue,
            }
        }
        found.sort_by_key(|entry| std::cmp::Reverse(entry.0));

        if found.is_empty() {
            return Ok(ToolResult::ok("No files matched."));
        }
        let body = found
            .iter()
            .map(|(_, p)| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult::ok(body))
    }
}

/// Join a base directory and a glob pattern into a single pattern string.
fn join_pattern(base: &std::path::Path, pattern: &str) -> String {
    let base = base.display().to_string();
    if pattern.starts_with('/') || pattern.starts_with('~') {
        return pattern.to_string();
    }
    let base = base.trim_end_matches('/');
    format!("{base}/{pattern}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_patterns() {
        let p = std::path::Path::new("/tmp/x");
        assert_eq!(join_pattern(p, "**/*.rs"), "/tmp/x/**/*.rs");
        assert_eq!(join_pattern(p, "/abs/*.rs"), "/abs/*.rs");
        assert_eq!(join_pattern(std::path::Path::new("/a/"), "b"), "/a/b");
    }
}
