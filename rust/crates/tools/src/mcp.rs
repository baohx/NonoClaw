//! Minimal MCP (Model Context Protocol) stdio client + dynamic tool adapter.
//!
//! Connects to MCP servers defined in an mcp-config file (Claude Code
//! `{"mcpServers": {...}}` shape), performs the `initialize` handshake, lists
//! tools, and wraps each as a [`Tool`] named `mcp__<server>__<tool>`. Mirrors
//! the role of `src/services/mcp/` + `src/tools/MCPTool/`.
//!
//! Transport is newline-delimited JSON-RPC 2.0 over the server child's stdio.
//! A background reader task demultiplexes responses by `id` (notifications are
//! ignored).

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use nonoclaw_core::{
    Error, ExtensionDescriptor, ExtensionDiagnostic, ExtensionDiagnosticSeverity, ExtensionKind,
    ExtensionSourceKind, ExtensionStatus, PermissionDecision, PermissionResult, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};
use crate::ToolRegistry;

const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// One MCP server config entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(rename = "type", default = "default_stdio")]
    pub kind: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_stdio() -> String {
    "stdio".into()
}

/// Parsed mcp-config file: `{ "mcpServers": { "<name>": {...} } }`.
#[derive(Debug, Deserialize)]
struct McpConfigFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: HashMap<String, McpServerConfig>,
}

/// Load server configs from a JSON file.
pub fn load_config(path: &Path) -> Result<Vec<(String, McpServerConfig)>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("read mcp-config {}: {e}", path.display())))?;
    let parsed: McpConfigFile =
        serde_json::from_str(&text).map_err(|e| Error::Config(format!("parse mcp-config: {e}")))?;
    Ok(parsed.mcp_servers.into_iter().collect())
}

/// A live MCP connection.
pub struct McpClient {
    writer: std::sync::Arc<AsyncMutex<ChildStdin>>,
    pending: std::sync::Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    child: Mutex<Option<Child>>,
    server_name: String,
}

impl McpClient {
    /// Spawn the server process and perform the initialize handshake.
    pub async fn spawn(server_name: &str, cfg: &McpServerConfig) -> Result<std::sync::Arc<Self>> {
        let mut command = Command::new(&cfg.command);
        command.args(&cfg.args);
        for (k, v) in &cfg.env {
            command.env(k, v);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(|e| Error::Tool {
            tool: "MCP".into(),
            message: format!("spawn server `{server_name}` (`{}`): {e}", cfg.command),
        })?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");

        let pending: std::sync::Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            std::sync::Arc::new(Mutex::new(HashMap::new()));
        let pending_r = std::sync::Arc::clone(&pending);
        tokio::spawn(async move {
            read_loop(stdout, pending_r).await;
        });

        let client = std::sync::Arc::new(McpClient {
            writer: std::sync::Arc::new(AsyncMutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            child: Mutex::new(Some(child)),
            server_name: server_name.to_string(),
        });

        // initialize handshake
        let init = client.initialize();
        tokio::time::timeout(INIT_TIMEOUT, init)
            .await
            .map_err(|_| Error::Tool {
                tool: "MCP".into(),
                message: format!("initialize timeout for `{server_name}`"),
            })??;
        let _ = client.notify("notifications/initialized", json!({})).await;
        Ok(client)
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    async fn initialize(&self) -> Result<Value> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "nonoclaw", "version": "0.1.0"}
            }),
        )
        .await
    }

    /// List the server's tools.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for t in tools {
            out.push(McpToolDef {
                name: t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}})),
            });
        }
        Ok(out)
    }

    /// Invoke a tool; returns (text, is_error).
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<(String, bool)> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut text = String::new();
        if let Some(blocks) = result.get("content").and_then(|v| v.as_array()) {
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(s) = b.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(s);
                    }
                }
            }
        }
        Ok((text, is_error))
    }

    /// List available prompts from the MCP server (MCP `prompts/list`).
    pub async fn list_prompts(&self) -> Result<Vec<McpPromptDef>> {
        let result = self.request("prompts/list", json!({})).await?;
        let prompts = result
            .get("prompts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for p in prompts {
            out.push(McpPromptDef {
                name: p
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: p
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        Ok(out)
    }

    /// Get a specific prompt's content (MCP `prompts/get`).
    pub async fn get_prompt(&self, name: &str, arguments: Value) -> Result<String> {
        let result = self
            .request("prompts/get", json!({"name": name, "arguments": arguments}))
            .await?;
        let mut text = String::new();
        if let Some(messages) = result.get("messages").and_then(|v| v.as_array()) {
            for msg in messages {
                if let Some(content) = msg.get("content").and_then(|v| v.as_object()) {
                    if content.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = content.get("text").and_then(|v| v.as_str()) {
                            text.push_str(t);
                            text.push('\n');
                        }
                    }
                }
            }
        }
        Ok(text)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (otx, orx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, otx);
        let req = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let line = serde_json::to_string(&req).map_err(|e| Error::Tool {
            tool: "MCP".into(),
            message: format!("encode: {e}"),
        })?;
        {
            let mut w = self.writer.lock().await;
            w.write_all(line.as_bytes())
                .await
                .map_err(|e| Error::Tool {
                    tool: "MCP".into(),
                    message: format!("write: {e}"),
                })?;
            w.write_all(b"\n").await.map_err(|e| Error::Tool {
                tool: "MCP".into(),
                message: format!("write nl: {e}"),
            })?;
        }
        match tokio::time::timeout(CALL_TIMEOUT, orx).await {
            Ok(Ok(v)) => {
                if let Some(err) = v.get("error") {
                    return Err(Error::Tool {
                        tool: "MCP".into(),
                        message: format!(
                            "{}: {}",
                            err.get("type").and_then(|x| x.as_str()).unwrap_or("error"),
                            err.get("message").and_then(|x| x.as_str()).unwrap_or("")
                        ),
                    });
                }
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Err(_)) => Err(Error::Tool {
                tool: "MCP".into(),
                message: format!("server `{}` closed the response channel", self.server_name),
            }),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(Error::Tool {
                    tool: "MCP".into(),
                    message: format!("timeout calling `{method}` on `{}`", self.server_name),
                })
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let req = json!({"jsonrpc":"2.0","method":method,"params":params});
        let line = serde_json::to_string(&req).map_err(|e| Error::Tool {
            tool: "MCP".into(),
            message: format!("encode: {e}"),
        })?;
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Tool {
                tool: "MCP".into(),
                message: format!("write: {e}"),
            })?;
        w.write_all(b"\n").await.map_err(|e| Error::Tool {
            tool: "MCP".into(),
            message: format!("write: {e}"),
        })?;
        Ok(())
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().expect("child lock").take() {
            let _ = child.start_kill();
            let _ = child.try_wait();
        }
    }
}

async fn read_loop(
    stdout: ChildStdout,
    pending: std::sync::Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
            if let Some(tx) = pending.lock().expect("pending lock").remove(&id) {
                let _ = tx.send(v);
            }
        }
        // notifications / server-initiated messages: ignored in Phase 3.
    }
    pending.lock().expect("pending lock").clear();
}

/// A discovered MCP prompt definition.
#[derive(Debug, Clone)]
pub struct McpPromptDef {
    pub name: String,
    pub description: String,
}

/// A discovered MCP tool definition.
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A [`Tool`] wrapper around one MCP-provided tool.
pub struct McpTool {
    full_name: String,
    raw_name: String,
    description: String,
    input_schema: Value,
    client: std::sync::Arc<McpClient>,
}

impl McpTool {
    fn new(server: &str, def: McpToolDef, client: std::sync::Arc<McpClient>) -> Self {
        let description = if def.description.is_empty() {
            format!("MCP tool {} ({})", def.name, server)
        } else {
            def.description
        };
        McpTool {
            full_name: format!("mcp__{server}__{}", def.name),
            raw_name: def.name,
            description,
            input_schema: def.input_schema,
            client,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    // MCP tools reuse their server-supplied description as the model-facing text.
    fn prompt(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false // unknown; gate by default
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionDecision::ask("run MCP server tool")
    }
    async fn call(
        &self,
        input: Value,
        _ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let (text, is_error) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            r = self.client.call_tool(&self.raw_name, input) => r?,
        };
        let data = if is_error {
            format!("[MCP tool error] {text}")
        } else {
            text
        };
        Ok(ToolResult::ok(data))
    }
}

/// Spawn each configured server, discover its tools, and register them.
/// Per-server spawn/list failures are logged and skipped (non-fatal).
pub async fn register(registry: &mut ToolRegistry, configs: &[(String, McpServerConfig)]) {
    for (name, cfg) in configs {
        let source = format!("mcp-config:{name}");
        let mut descriptor = ExtensionDescriptor::new(
            ExtensionKind::Mcp,
            name.clone(),
            source.clone(),
            ExtensionSourceKind::Explicit,
            100,
        );
        match McpClient::spawn(name, cfg).await {
            Ok(client) => match client.list_tools().await {
                Ok(defs) => {
                    let n = defs.len();
                    for d in defs {
                        registry.register(std::sync::Arc::new(McpTool::new(
                            name,
                            d,
                            std::sync::Arc::clone(&client),
                        )));
                    }
                    descriptor.detail = Some(format!("connected; {n} tool(s)"));
                    registry.add_extension_descriptor(descriptor);
                    tracing::info!("MCP server `{name}`: registered {n} tool(s)");
                }
                Err(error) => {
                    descriptor.status = ExtensionStatus::Failed;
                    descriptor.detail = Some(error.to_string());
                    registry.add_extension_descriptor(descriptor);
                    registry.add_extension_diagnostic(mcp_failure(name, &source, &error));
                    tracing::warn!("MCP server `{name}` tools/list failed: {error}");
                }
            },
            Err(error) => {
                descriptor.status = ExtensionStatus::Failed;
                descriptor.detail = Some(error.to_string());
                registry.add_extension_descriptor(descriptor);
                registry.add_extension_diagnostic(mcp_failure(name, &source, &error));
                tracing::warn!("MCP server `{name}` spawn failed: {error}");
            }
        }
    }
}

fn mcp_failure(name: &str, source: &str, error: &Error) -> ExtensionDiagnostic {
    ExtensionDiagnostic {
        severity: ExtensionDiagnosticSeverity::Error,
        code: "mcp_load_failed".into(),
        kind: ExtensionKind::Mcp,
        name: Some(name.to_string()),
        source: Some(source.to_string()),
        message: format!("MCP server `{name}` failed: {error}"),
        suggestion: "check the command and environment; other extensions remain available".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_mcp_is_isolated_from_core_registry() {
        let (mut registry, _) = crate::register_all();
        let core_count = registry.len();
        let configs = vec![(
            "broken".to_string(),
            McpServerConfig {
                kind: "stdio".into(),
                command: "nonoclaw-command-that-does-not-exist".into(),
                args: vec![],
                env: HashMap::new(),
            },
        )];
        register(&mut registry, &configs).await;
        assert_eq!(registry.len(), core_count);
        assert!(registry.find("Read").is_some());
        assert!(registry
            .extension_descriptors()
            .iter()
            .any(|descriptor| descriptor.name == "broken"
                && descriptor.status == ExtensionStatus::Failed));
        assert_eq!(registry.extension_diagnostics().len(), 1);
    }
}
