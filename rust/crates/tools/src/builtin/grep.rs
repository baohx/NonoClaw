//! Grep tool. Mirrors `src/tools/GrepTool/`. Delegates to the `rg` (ripgrep)
//! binary — mirrors `src/utils/ripgrep.ts` which also shells out to rg.

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

/// Wall-clock budget for a single `rg` invocation.
const GREP_TIMEOUT_MS: u64 = 120_000;

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
    fn snippet(&self) -> String {
        "Search file contents with a regex (ripgrep)".to_string()
    }
    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Use Grep over Bash-with-`rg` for content searches: faster, respects .gitignore, structured output.",
            "Combine Grep with Read to inspect the surrounding context of a match.",
        ]
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
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let pattern = input["pattern"].as_str().ok_or_else(|| Error::Tool {
            tool: "Grep".into(),
            message: "missing required field `pattern`".into(),
        })?;
        let mode = OutputMode::from_str(input["output_mode"].as_str().unwrap_or(""));
        let path = input["path"]
            .as_str()
            .map(|p| ctx.environment.resolve_user_path(ctx.cwd, p))
            .unwrap_or_else(|| ctx.environment.effective_cwd(ctx.cwd));

        let mut argv: Vec<String> = vec![
            "rg".to_string(),
            "--color".to_string(),
            "never".to_string(),
            "--no-heading".to_string(),
        ];
        if input["-i"].as_bool().unwrap_or(false) {
            argv.push("-i".to_string());
        }
        if input["multiline"].as_bool().unwrap_or(false) {
            argv.push("-U".to_string());
            argv.push("--multiline-dotall".to_string());
        }
        if let Some(g) = input["glob"].as_str() {
            argv.push("--glob".to_string());
            argv.push(g.to_string());
        }
        if let Some(t) = input["type"].as_str() {
            argv.push("--type".to_string());
            argv.push(t.to_string());
        }
        match mode {
            OutputMode::Content => {
                let show_ln = input["-n"].as_bool().unwrap_or(true);
                if show_ln {
                    argv.push("-n".to_string());
                }
            }
            OutputMode::FilesWithMatches => {
                argv.push("--files-with-matches".to_string());
            }
            OutputMode::Count => {
                argv.push("-c".to_string());
            }
        }
        argv.push(pattern.to_string());
        argv.push(path.display().to_string());

        let output = ctx
            .environment
            .run_argv(&argv, ctx.cwd, GREP_TIMEOUT_MS, cancel)
            .await
            .map_err(|e| Error::Tool {
                tool: "Grep".into(),
                message: format!(
                    "failed to run `rg` (is ripgrep installed in this environment?): {e}"
                ),
            })?;

        let stdout = &output.stdout;
        let stderr = output.stderr.trim().to_string();
        // rg exit code 1 = no matches (not an error); anything else = error.
        if output.exit_code != 0 && output.exit_code != 1 {
            let hint = if stderr.contains("not found") || stderr.contains("No such file") {
                " (ripgrep is not installed in this environment; install it or use the Bash tool with `grep -rn` instead)"
            } else {
                ""
            };
            return Err(Error::Tool {
                tool: "Grep".into(),
                message: format!("{stderr}{hint}"),
            });
        }
        if stdout.is_empty() {
            return Ok(ToolResult::ok("No matches found."));
        }
        Ok(ToolResult::ok(stdout.to_string()))
    }
}
