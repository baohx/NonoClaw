//! Grep tool. Mirrors `src/tools/GrepTool/`. Delegates to the `rg` (ripgrep)
//! binary — mirrors `src/utils/ripgrep.ts` which also shells out to rg.

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::builtin::resolve_path;
use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "A powerful search tool built on ripgrep\n\nUsage:\n- ALWAYS use Grep for search tasks. NEVER invoke `grep` or `rg` as a Bash command. The Grep tool has been optimized for correct permissions and access.\n- Supports full regex syntax (e.g. \"log.*Error\", \"function\\s+\\w+\")\n- Filter files with glob parameter (e.g. \"*.js\", \"**/*.tsx\") or type parameter (e.g. \"js\", \"py\", \"rust\")\n- Output modes: \"content\" shows matching lines, \"files_with_matches\" shows only file paths (default), \"count\" shows match counts\n- Pattern syntax: Uses ripgrep (not grep) - literal braces need escaping\n- Multiline matching: by default patterns match within single lines only. For cross-line patterns use `multiline: true`";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    FilesWithMatches,
    Count,
}

impl OutputMode {
    fn from_str(s: &str) -> Self {
        match s {
            "content" => OutputMode::Content,
            "count" => OutputMode::Count,
            _ => OutputMode::FilesWithMatches,
        }
    }
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "Grep"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Search file contents with ripgrep."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type":"string","description":"The regular expression to search for"},
                "path": {"type":"string","description":"File or directory to search (absolute or relative to cwd)"},
                "glob": {"type":"string","description":"Glob filter, e.g. \"*.js\""},
                "type": {"type":"string","description":"File type filter, e.g. \"rust\""},
                "output_mode": {"type":"string","enum":["content","files_with_matches","count"],"description":"Default files_with_matches"},
                "-i": {"type":"boolean","description":"Case-insensitive"},
                "-n": {"type":"boolean","description":"Show line numbers (content mode, default true)"},
                "multiline": {"type":"boolean","description":"Enable multiline matching"}
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
            tool: "Grep".into(),
            message: "missing required field `pattern`".into(),
        })?;
        let mode = OutputMode::from_str(input["output_mode"].as_str().unwrap_or(""));
        let path = input["path"]
            .as_str()
            .map(|p| resolve_path(ctx.cwd, p))
            .unwrap_or_else(|| ctx.cwd.to_path_buf());

        let mut cmd = Command::new("rg");
        cmd.arg("--color").arg("never").arg("--no-heading");
        if input["-i"].as_bool().unwrap_or(false) {
            cmd.arg("-i");
        }
        if input["multiline"].as_bool().unwrap_or(false) {
            cmd.arg("-U").arg("--multiline-dotall");
        }
        if let Some(g) = input["glob"].as_str() {
            cmd.arg("--glob").arg(g);
        }
        if let Some(t) = input["type"].as_str() {
            cmd.arg("--type").arg(t);
        }
        match mode {
            OutputMode::Content => {
                let show_ln = input["-n"].as_bool().unwrap_or(true);
                if show_ln {
                    cmd.arg("-n");
                }
            }
            OutputMode::FilesWithMatches => {
                cmd.arg("--files-with-matches");
            }
            OutputMode::Count => {
                cmd.arg("-c");
            }
        }
        cmd.arg(pattern).arg(&path);

        let output = cmd.output().await.map_err(|e| Error::Tool {
            tool: "Grep".into(),
            message: format!("failed to run `rg` (is ripgrep installed?): {e}"),
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // rg exit code 1 = no matches (not an error); 2 = real error.
        let code = output.status.code().unwrap_or(0);
        if code == 2 {
            return Err(Error::Tool {
                tool: "Grep".into(),
                message: stderr.trim().to_string(),
            });
        }
        if stdout.is_empty() {
            return Ok(ToolResult::ok("No matches found."));
        }
        Ok(ToolResult::ok(stdout.to_string()))
    }
}
