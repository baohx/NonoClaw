//! Write tool. Mirrors `src/tools/FileWriteTool/`.

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::builtin::{read::require_str, resolve_path};
use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Writes a file to the local filesystem.\n\nUsage:\n- This tool will overwrite the existing file if there is one at the provided path.\n- If this is an existing file, you MUST use the Read tool first to read the file's contents. This tool will fail if you did not read the file first.\n- Prefer the Edit tool for modifying existing files — it only sends the diff. Only use this tool to create new files or for complete rewrites.\n- NEVER create documentation files (*.md) or README files unless explicitly requested by the User.\n- Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked.";

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "Write"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Write a file to the local filesystem."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type":"string","description":"The absolute path to the file to write (must be absolute, not relative)"},
                "content": {"type":"string","description":"The content to write to the file"}
            },
            "required": ["file_path", "content"]
        })
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionDecision::ask("write a file")
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let file_path = require_str(&input, "file_path")?;
        let content = input["content"].as_str().ok_or_else(|| Error::Tool {
            tool: "Write".into(),
            message: "missing required string field `content`".into(),
        })?;
        let path = resolve_path(ctx.cwd, file_path);

        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        // Create parent directories as needed.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| Error::Tool {
                        tool: "Write".into(),
                        message: format!("create_dir_all {}: {e}", parent.display()),
                    })?;
            }
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| Error::Tool {
                tool: "Write".into(),
                message: format!("{}: {e}", path.display()),
            })?;

        Ok(ToolResult::ok(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path.display()
        )))
    }
}
