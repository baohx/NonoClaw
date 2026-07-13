//! Read tool. Mirrors `src/tools/FileReadTool/` (`FileReadTool.ts`, `prompt.ts`).

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::builtin::resolve_path;
use crate::tool::{Tool, ToolCtx, ToolResult};

const MAX_LINES: usize = 2000;

const PROMPT: &str = "Reads a file from the local filesystem. You can access any file directly by using this tool.\nAssume this tool is able to read all files on the machine. If the User provides a path to a file assume that path is valid. It is okay to read a file that does not exist; an error will be returned.\n\nUsage:\n- The file_path parameter must be an absolute path, not a relative path\n- By default, it reads up to 2000 lines starting from the beginning of the file\n- You can optionally specify a line offset and limit (especially handy for long files), but it's recommended to read the whole file by not providing these parameters\n- Results are returned using cat -n format, with line numbers starting at 1\n- This tool allows reading images (PNG, JPG, etc.) as content is presented visually (multimodal).\n- This tool can read Jupyter notebooks (.ipynb) and returns all cells with their outputs.\n- This tool can only read files, not directories. To read a directory, use an `ls` command via the Bash tool.\n- If you read a file that exists but has empty contents you will receive a system reminder warning in place of file contents.";

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "Read"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Read a file from the local filesystem."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "The line number to start reading from (1-based)"
                },
                "limit": {
                    "type": "integer",
                    "description": "The number of lines to read (default 2000)"
                }
            },
            "required": ["file_path"]
        })
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        usize::MAX
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
        let file_path = require_str(&input, "file_path")?;
        let offset = input["offset"].as_u64().map(|n| n as usize);
        let limit = input["limit"].as_u64().map(|n| n as usize);
        let path = resolve_path(ctx.cwd, file_path);

        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let bytes = tokio::fs::read(&path).await.map_err(|e| Error::Tool {
            tool: "Read".into(),
            message: format!("{}: {e}", path.display()),
        })?;

        // Binary detection: a NUL byte in the first chunk means not text.
        if bytes.iter().take(8000).any(|&b| b == 0) {
            return Ok(ToolResult::ok(format!(
                "({} — file appears to be binary, skipped)",
                path.display()
            )));
        }

        let content = String::from_utf8_lossy(&bytes);
        if content.is_empty() {
            return Ok(ToolResult::ok(format!(
                "<system-reminder>File exists but is empty: {}</system-reminder>",
                path.display()
            )));
        }

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.unwrap_or(1).saturating_sub(1).min(lines.len());
        let take = limit.unwrap_or(MAX_LINES);

        let mut out = String::new();
        for (i, line) in lines.iter().enumerate().skip(start).take(take) {
            // cat -n style: 6-wide right-justified number + tab + content.
            out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
        }
        Ok(ToolResult::ok(out))
    }
}

pub(crate) fn require_str<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key].as_str().ok_or_else(|| Error::Tool {
        tool: "input".into(),
        message: format!("missing required string field `{key}`"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn reads_with_line_numbers_and_offset() {
        let tmp = tempfile_dir();
        let file = tmp.join("a.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let tool = ReadTool;
        let opts = crate::tool::ToolOptions {
            model: "x".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let cwd: &Path = &tmp;
        let ctx = ToolCtx {
            cwd,
            options: &opts,
            cancel: &cancel,
            subagent: None,
            question: None,
            background_registry: None,
        };
        let res = tool
            .call(
                json!({"file_path": file.to_str().unwrap(), "offset": 2, "limit": 1}),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(res.data.contains("     2\tbeta"));
        assert!(!res.data.contains("alpha"));
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nonoclaw-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
