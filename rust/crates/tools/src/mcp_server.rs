//! Run NonoClaw as an MCP server (the inverse of [`crate::mcp`]). Mirrors
//! `src/entrypoints/mcp.ts`: the process speaks JSON-RPC 2.0 over stdio and
//! exposes its built-in tools via `tools/list` + `tools/call`, so another MCP
//! client (Claude Desktop, another agent, etc.) can drive them.
//!
//! Only the process's own stdout carries JSON-RPC; tool child processes capture
//! their stdout, and tracing goes to stderr, so the wire stays clean. The server
//! trusts its client and runs tools under `BypassPermissions` (the client owns
//! gating). `Agent` is excluded (no engine/client available to spawn subagents).

use std::io::Write;
use std::path::Path;

use nonoclaw_core::{PermissionMode, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::tool::{ToolCtx, ToolOptions};

/// JSON-RPC error object.
fn rpc_error(code: i64, message: &str) -> Value {
    json!({"code": code, "message": message})
}

/// Run the MCP server loop on stdin/stdout until EOF.
pub async fn serve_stdin(registry: &crate::ToolRegistry, cwd: &Path) -> Result<()> {
    // Agent can't run here (no engine/client), so don't advertise it.
    let served = registry.filtered(&["Agent"]);

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break; // EOF — client gone
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            // Not JSON; skip (a real server would return a parse error if id present).
            continue;
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let result: std::result::Result<Value, Value> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "nonoclaw", "version": env!("CARGO_PKG_VERSION")}
            })),
            "tools/list" => {
                let tools: Vec<Value> = served
                    .definitions(None)
                    .into_iter()
                    .map(|d| {
                        json!({
                            "name": d.name,
                            "description": d.description,
                            "inputSchema": d.input_schema,
                        })
                    })
                    .collect();
                Ok(json!({"tools": tools}))
            }
            "tools/call" => call_tool(&served, cwd, &params).await,
            // Notifications (no id) — acknowledge nothing.
            "notifications/initialized" => {
                continue;
            }
            _ => Err(rpc_error(-32601, &format!("method not found: {method}"))),
        };

        // Only respond to requests that carried an id.
        if let Some(id) = id {
            let resp = match result {
                Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
                Err(e) => json!({"jsonrpc":"2.0","id":id,"error":e}),
            };
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{resp}");
            let _ = out.flush();
        }
    }
    Ok(())
}

/// Handle a `tools/call`: find the tool, run it, return MCP content blocks.
async fn call_tool(
    registry: &crate::ToolRegistry,
    cwd: &Path,
    params: &Value,
) -> std::result::Result<Value, Value> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| rpc_error(-32602, "missing `name` in params"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let Some(tool) = registry.find(name) else {
        return Err(rpc_error(-32602, &format!("unknown tool: {name}")));
    };

    let opts = ToolOptions {
        model: String::new(),
        permission_mode: PermissionMode::BypassPermissions,
        is_non_interactive: true,
        max_budget_usd: None,
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = ToolCtx {
        cwd,
        options: &opts,
        cancel: &cancel,
        subagent: None,
        question: None,
    };

    let outcome = tool.call(args, &ctx, cancel.clone()).await;
    Ok(match outcome {
        Ok(r) => json!({
            "content": [{"type":"text","text": r.data}],
            "isError": false,
        }),
        Err(e) => json!({
            "content": [{"type":"text","text": format!("Error: {e}")}],
            "isError": true,
        }),
    })
}
