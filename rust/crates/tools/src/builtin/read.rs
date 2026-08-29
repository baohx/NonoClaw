//! Read tool. Mirrors `src/tools/FileReadTool/` (`FileReadTool.ts`, `prompt.ts`).

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::builtin::resolve_path;
use crate::tool::{Tool, ToolCtx, ToolResult};

const MAX_LINES: usize = 2000;
/// Hard cap on a single Read result (≈16k tokens): a huge dump (e.g. a secret
/// or minified file) must not dominate the context on its own.
const MAX_RESULT_CHARS: usize = 64_000;
/// Cap for a single line — credential material often arrives as one enormous
/// line; truncate it so the redaction layer sees a bounded payload.
const MAX_LINE_CHARS: usize = 4_000;

const PROMPT: &str = "Reads a file from the local filesystem. You can access any file directly by using this tool.\nAssume this tool is able to read all files on the machine. If the User provides a path to a file assume that path is valid. It is okay to read a file that does not exist; an error will be returned.\n\nUsage:\n- The file_path parameter can be an absolute path or a path relative to cwd (e.g. paths returned by Glob or Grep)\n- By default, it reads up to 2000 lines starting from the beginning of the file\n- You can optionally specify a line offset and limit (especially handy for long files), but it's recommended to read the whole file by not providing these parameters\n- Results are returned using cat -n format, with line numbers starting at 1\n- This tool allows reading images (PNG, JPG, etc.) as content is presented visually (multimodal).\n- This tool can read Jupyter notebooks (.ipynb) and returns all cells with their outputs.\n- This tool can only read files, not directories. To read a directory, use an `ls` command via the Bash tool.\n- If you read a file that exists but has empty contents you will receive a system reminder warning in place of file contents.";

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
    fn snippet(&self) -> String {
        "Read a file with optional offset/limit".to_string()
    }
    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Read a file before editing it. Use offset/limit on large files instead of dumping the whole body.",
        ]
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The path to the file to read (absolute or relative to cwd)"
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
        MAX_RESULT_CHARS
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

        // Gate ③: refuse to read locations that routinely hold credentials
        // (SSH keys, cloud tokens, dotenv, private-key material). This runs in
        // every permission mode, bypass included — the agent must use a
        // scrubbed, non-secret source for secrets.
        if crate::sensitive::is_sensitive_path(&path) {
            return Ok(ToolResult::error(format!(
                "refusing to read {}: path may contain credentials (SSH/cloud/dotenv/private-key). \
                 Read does not expose secret material; provide the value via env vars or a \
                 non-secret, scrubbed source instead.",
                path.display()
            )));
        }

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
            // Truncate pathologically long lines (credential/minified dumps).
            let shown = if line.chars().count() > MAX_LINE_CHARS {
                let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
                format!("{cut}…[{} chars truncated]", line.chars().count() - MAX_LINE_CHARS)
            } else {
                (*line).to_string()
            };
            out.push_str(&format!("{:>6}\t{shown}\n", i + 1));
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
            tool_use_id: "read-test-call",
            task_scope: Some("read-test"),
            subagent: None,
            graph_runner: None,
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

    #[tokio::test]
    async fn refuses_sensitive_paths_in_every_mode() {
        let tmp = tempfile_dir();
        let secret = tmp.join(".env");
        std::fs::write(&secret, "PASSWORD=supersecret\n").unwrap();
        let tool = ReadTool;
        let opts = crate::tool::ToolOptions {
            model: "x".into(),
            // Bypass must NOT bypass the credential denylist.
            permission_mode: nonoclaw_core::PermissionMode::BypassPermissions,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let cwd: &Path = &tmp;
        let ctx = ToolCtx {
            cwd,
            options: &opts,
            cancel: &cancel,
            tool_use_id: "read-test-secret",
            task_scope: Some("read-test"),
            subagent: None,
            graph_runner: None,
            question: None,
            background_registry: None,
        };
        let res = tool
            .call(
                json!({"file_path": ".env"}),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(res.data.contains("refusing to read"), "got: {}", res.data);
        assert!(!res.data.contains("supersecret"));
    }

    #[tokio::test]
    async fn truncates_pathologically_long_lines() {
        let tmp = tempfile_dir();
        let file = tmp.join("long.txt");
        let long_line = "x".repeat(MAX_LINE_CHARS + 500);
        std::fs::write(&file, format!("{long_line}\n")).unwrap();
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
            tool_use_id: "read-test-long",
            task_scope: Some("read-test"),
            subagent: None,
            graph_runner: None,
            question: None,
            background_registry: None,
        };
        let res = tool
            .call(
                json!({"file_path": "long.txt"}),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(res.data.contains("[500 chars truncated]"), "got: {}", &res.data[..200]);
        assert!(res.data.len() < MAX_LINE_CHARS + 200);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nonoclaw-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
