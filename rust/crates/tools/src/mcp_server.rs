//! Run NonoClaw as an MCP server (the inverse of [`crate::mcp`]). Mirrors
//! `src/entrypoints/mcp.ts`: the process speaks JSON-RPC 2.0 over stdio and
//! exposes its built-in tools via `tools/list` + `tools/call`, so another MCP
//! client (Claude Desktop, another agent, etc.) can drive them.
//!
//! Only the process's own stdout carries JSON-RPC; tool child processes capture
//! their stdout, and tracing goes to stderr, so the wire stays clean. The server
//! trusts its client and runs tools under `BypassPermissions` (the client owns
//! gating). `Agent` is excluded (no engine/client available to spawn subagents).

use std::path::Path;

use nonoclaw_core::{PermissionMode, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::tool::{ToolCtx, ToolOptions};

/// JSON-RPC error object.
fn rpc_error(code: i64, message: &str) -> Value {
    json!({"code": code, "message": message})
}

/// Run the MCP server loop on stdin/stdout until EOF.
pub async fn serve_stdin(registry: &crate::ToolRegistry, cwd: &Path) -> Result<()> {
    serve_io(registry, cwd, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Transport-generic MCP loop. Keeping framing here makes stdio behavior
/// characterizable with in-memory streams and no external process.
async fn serve_io<R, W>(
    registry: &crate::ToolRegistry,
    cwd: &Path,
    reader: R,
    mut writer: W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Agent can't run here (no engine/client), so don't advertise it.
    let served = registry.filtered(&["Agent"]);

    let mut reader = BufReader::new(reader);
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
            writer.write_all(format!("{resp}\n").as_bytes()).await?;
            writer.flush().await?;
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
        task_scope: Some("mcp"),
        subagent: None,
        question: None,
        background_registry: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Minimal initialize → list → call flow over the real JSONL MCP adapter.
    /// Feature Preservation Matrix: §2.2 MCP server and §3.2; Requirements 1.3-1.4.
    #[tokio::test]
    async fn mcp_server_minimal_success_path() {
        let cwd = std::env::temp_dir().join(format!("nonoclaw-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("fixture.txt");
        std::fs::write(&file, "fixture body\n").unwrap();

        let (registry, _) = crate::builtin::register_all();
        let (client, server) = tokio::io::duplex(128 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let server_cwd = cwd.clone();
        let task = tokio::spawn(async move {
            serve_io(&registry, &server_cwd, server_read, server_write).await
        });

        let (client_read, mut client_write) = tokio::io::split(client);
        let mut responses = BufReader::new(client_read).lines();
        client_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        let initialized: Value =
            serde_json::from_str(&responses.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], "2024-11-05");

        client_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n")
            .await
            .unwrap();
        let listed: Value =
            serde_json::from_str(&responses.next_line().await.unwrap().unwrap()).unwrap();
        let names: Vec<_> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(names.len(), 19);
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"TaskOutput"));
        assert!(names.contains(&"TaskStop"));
        assert!(!names.contains(&"Agent"));

        let call = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "Read", "arguments": {"file_path": file}}
        });
        client_write
            .write_all(format!("{call}\n").as_bytes())
            .await
            .unwrap();
        let called: Value =
            serde_json::from_str(&responses.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(called["result"]["isError"], false);
        assert!(called["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("fixture body"));

        client_write.shutdown().await.unwrap();
        drop(client_write);
        task.abort();
        let _ = task.await;
    }
}
