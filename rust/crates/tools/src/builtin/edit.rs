//! Edit tool. Mirrors `src/tools/FileEditTool/`. Performs exact string
//! replacement; fails when `old_string` is absent or (without `replace_all`)
//! not unique.

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::builtin::{read::require_str, resolve_path};
use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Performs exact string replacements in files.\n\nUsage:\n- You must use your `Read` tool at least once in the conversation before editing. This tool will error if you attempt an edit without reading the file.\n- When editing text from Read tool output, ensure you preserve the exact indentation (tabs/spaces) as it appears AFTER the line number prefix.\n- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.\n- Only use emojis if the user explicitly requests it.\n- The edit will FAIL if `old_string` is not unique in the file. Either provide a larger string with more surrounding context to make it unique or use `replace_all` to change every instance of `old_string`.\n- Use `replace_all` for replacing and renaming strings across the file.";

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "Edit"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Performs exact string replacements in files."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type":"string","description":"The absolute path to the file to modify"},
                "old_string": {"type":"string","description":"The text to replace"},
                "new_string": {"type":"string","description":"The text to replace it with (must be different from old_string)"},
                "replace_all": {"type":"boolean","description":"Replace all occurrences of old_string (default false)"}
            },
            "required": ["file_path", "old_string", "new_string"]
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
        PermissionDecision::ask("edit a file")
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let file_path = require_str(&input, "file_path")?;
        let old_string = require_str(&input, "old_string")?;
        let new_string = require_str(&input, "new_string")?;
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);
        let path = resolve_path(ctx.cwd, file_path);

        if old_string == new_string {
            return Err(Error::Tool {
                tool: "Edit".into(),
                message: "new_string must be different from old_string".into(),
            });
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| Error::Tool {
                tool: "Edit".into(),
                message: format!("{}: {e}", path.display()),
            })?;

        let count = content.matches(old_string).count();
        if count == 0 {
            return Err(Error::Tool {
                tool: "Edit".into(),
                message: format!(
                    "old_string not found in {}. Make sure it matches exactly, including whitespace.",
                    path.display()
                ),
            });
        }
        if count > 1 && !replace_all {
            return Err(Error::Tool {
                tool: "Edit".into(),
                message: format!(
                    "old_string is not unique ({count} matches) in {}. \
                     Provide more surrounding context or set replace_all=true.",
                    path.display()
                ),
            });
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        tokio::fs::write(&path, &new_content)
            .await
            .map_err(|e| Error::Tool {
                tool: "Edit".into(),
                message: format!("{}: {e}", path.display()),
            })?;

        let short_path = path
            .strip_prefix(std::env::current_dir().unwrap_or_default())
            .unwrap_or(&path);
        Ok(ToolResult::ok(format!(
            "{} ({} occurrence{})\n-{}\n+{}",
            short_path.display(),
            if replace_all { count } else { 1 },
            if count == 1 && !replace_all { "" } else { "s" },
            old_string,
            new_string,
        )))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn replace_single_unique() {
        let s = "foo bar baz";
        assert_eq!(s.replacen("bar", "QUX", 1), "foo QUX baz");
    }

    #[test]
    fn counts_matches() {
        assert_eq!("a a a".matches("a").count(), 3);
        assert_eq!("abc".matches("z").count(), 0);
    }
}
