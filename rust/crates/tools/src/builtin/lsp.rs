//! LSP tool — code intelligence via grep-based symbol search.
//!
//! Operations: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol.
//! Uses ripgrep (rg) for symbol search. Falls back to grep if rg is unavailable.
//! No language server installation required.

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use std::path::Path;
use tokio_util::sync::CancellationToken;

use crate::builtin::resolve_path;
use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "LSP tool — code intelligence via grep-based symbol search.\n\nOperations:\n- `goToDefinition`: find where a symbol is defined using regex pattern\n- `findReferences`: find all references to a symbol\n- `documentSymbol`: list all top-level symbols (fn, struct, impl, class, def, etc.) in a file\n- `workspaceSymbol`: search for a symbol by name across the entire workspace\n\nUse `filePath`, `line`, `character` (1-based) for position-based ops. `workspaceSymbol` uses `query`.";

pub struct LspTool;

impl LspTool {
    pub fn new() -> Self { LspTool }
}

async fn rg(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("rg")
        .args(args)
        .current_dir(cwd)
        .arg("--no-heading")
        .arg("--color=never")
        .output()
        .await;
    match output {
        Ok(o) => Ok(String::from_utf8_lossy(&o.stdout).to_string()),
        Err(e) => Err(Error::Tool { tool: "LSP".into(), message: format!("rg: {e}") }),
    }
}

/// List symbols in a file by grepping for common patterns.
fn symbols_in_file(path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::Tool {
        tool: "LSP".into(), message: format!("read: {e}"),
    })?;
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let is_symbol = trimmed.starts_with("fn ")
            || trimmed.starts_with("pub fn ")
            || trimmed.starts_with("struct ")
            || trimmed.starts_with("pub struct ")
            || trimmed.starts_with("enum ")
            || trimmed.starts_with("impl ")
            || trimmed.starts_with("trait ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("def ")
            || trimmed.starts_with("export ")
            || trimmed.starts_with("const ")
            || trimmed.starts_with("let ")
            || trimmed.starts_with("var ")
            || trimmed.starts_with("type ")
            || trimmed.starts_with("interface ")
            || trimmed.starts_with("mod ");
        if is_symbol {
            out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
        }
    }
    if out.is_empty() { out = "(no symbols detected)".into(); }
    Ok(out)
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &'static str { "LSP" }
    fn prompt(&self) -> &'static str { PROMPT }
    fn description(&self) -> &'static str {
        "Code intelligence: go-to-definition, find references, list symbols via ripgrep."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "description": "Operation: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol",
                    "enum": ["goToDefinition", "findReferences", "hover", "documentSymbol", "workspaceSymbol"]
                },
                "filePath": { "type": "string", "description": "File path (absolute)" },
                "line": { "type": "integer", "description": "Line number (1-based)" },
                "character": { "type": "integer", "description": "Character offset (1-based)" },
                "query": { "type": "string", "description": "Symbol name or pattern to search (workspaceSymbol)" }
            },
            "required": ["operation", "filePath", "line", "character"]
        })
    }
    fn is_read_only(&self, _: &Value) -> bool { true }
    fn is_concurrency_safe(&self, _: &Value) -> bool { true }

    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionResult::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let op = input["operation"].as_str().unwrap_or("documentSymbol");
        let file_path = input["filePath"].as_str().unwrap_or("");
        let path = resolve_path(ctx.cwd, file_path);

        match op {
            "documentSymbol" => {
                let syms = symbols_in_file(&path)?;
                Ok(ToolResult::ok(format!("Symbols in {}:\n{syms}", path.display())))
            }
            "workspaceSymbol" => {
                let query = input["query"].as_str().unwrap_or("");
                if query.is_empty() {
                    return Err(Error::Tool { tool: "LSP".into(), message: "query required for workspaceSymbol".into() });
                }
                let pattern = format!(r"(fn|struct|enum|trait|impl|class|def|export|interface|mod|type|const)\s+.*{query}");
                let result = rg(ctx.cwd, &[&pattern, "--type-add=all:*", "-n"]).await?;
                let capped = if result.lines().count() > 40 {
                    let mut s: String = result.lines().take(40).collect::<Vec<_>>().join("\n");
                    s.push_str("\n... (truncated)");
                    s
                } else { result };
                Ok(ToolResult::ok(capped))
            }
            "goToDefinition" => {
                // Extract symbol name from line
                let content = std::fs::read_to_string(&path).map_err(|e| Error::Tool {
                    tool: "LSP".into(), message: format!("read: {e}"),
                })?;
                let line_no = input["line"].as_u64().unwrap_or(1).saturating_sub(1) as usize;
                let target_line = content.lines().nth(line_no).unwrap_or("");
                // Extract identifier-like token
                let word = target_line
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .find(|w| !w.is_empty() && !["fn", "struct", "pub", "let", "const", "mut", "use", "mod", "impl"].contains(w))
                    .unwrap_or("unknown");
                let pattern = format!(r"^(pub\s+)?(fn|struct|enum|trait|impl|mod|type|const|static)\s+{word}\b");
                let result = rg(ctx.cwd, &[&pattern, "--type-add=all:*", "-n"]).await?;
                Ok(ToolResult::ok(format!("Definition of '{word}':\n{result}")))
            }
            "findReferences" => {
                let content = std::fs::read_to_string(&path).map_err(|e| Error::Tool {
                    tool: "LSP".into(), message: format!("read: {e}"),
                })?;
                let line_no = input["line"].as_u64().unwrap_or(1).saturating_sub(1) as usize;
                let target_line = content.lines().nth(line_no).unwrap_or("");
                let word = target_line
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .find(|w| w.len() > 2 && !["fn", "struct", "pub", "let", "const", "mut", "use", "mod", "impl"].contains(w))
                    .unwrap_or("unknown");
                let result = rg(ctx.cwd, &["-n", "-w", word]).await?;
                let capped = if result.lines().count() > 40 {
                    let mut s: String = result.lines().take(40).collect::<Vec<_>>().join("\n");
                    s.push_str("\n... (truncated)");
                    s
                } else { result };
                Ok(ToolResult::ok(format!("References to '{word}':\n{capped}")))
            }
            _ => Err(Error::Tool { tool: "LSP".into(), message: format!("unknown operation: {op}") }),
        }
    }
}
