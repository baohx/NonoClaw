//! Minimal MCP (Model Context Protocol) client + dynamic tool adapter.
//!
//! Connects to MCP servers defined in an mcp-config file (Claude Code
//! `{"mcpServers": {...}}` shape), performs the `initialize` handshake, lists
//! tools, and wraps each as a [`Tool`] named `mcp__<server>__<tool>`. Mirrors
//! the role of `src/services/mcp/` + `src/tools/MCPTool/`.
//!
//! Two transports: newline-delimited JSON-RPC 2.0 over a spawned server's
//! stdio (a background reader demultiplexes responses by `id`, notifications
//! are ignored), or the streamable-HTTP transport (MCP 2025-03-26) where each
//! message is POSTed to the server `url` and the response is JSON or SSE.

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
/// Protocol version negotiated on the streamable-HTTP transport.
const MCP_HTTP_PROTOCOL_VERSION: &str = "2025-03-26";

/// One MCP server config entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(rename = "type", default = "default_stdio")]
    pub kind: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Streamable-HTTP endpoint. Presence selects the HTTP transport; the
    /// legacy SSE transport (`"type": "sse"`) is not supported.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn default_stdio() -> String {
    "stdio".into()
}

/// Expand `${NONOCLAW_HOME}` / `${HOME}` / `${USERPROFILE}` placeholders in
/// MCP command, args and env values so portable deployments can reference the
/// bundled config directory without baking absolute paths into settings.json.
fn expand_placeholders(raw: &str) -> String {
    let mut out = raw.to_string();
    for key in ["NONOCLAW_HOME", "HOME", "USERPROFILE"] {
        let placeholder = format!("${{{key}}}");
        if out.contains(&placeholder) {
            if let Some(value) = std::env::var_os(key) {
                out = out.replace(&placeholder, &value.to_string_lossy());
            }
        }
    }
    out
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
///
/// The transport is either a spawned child process speaking newline-delimited
/// JSON-RPC over stdio, or a streamable-HTTP endpoint reached per request.
/// Higher-level operations (initialize / tools / prompts) are identical on
/// both.
#[derive(Clone)]
enum Transport {
    Stdio {
        writer: std::sync::Arc<AsyncMutex<ChildStdin>>,
        pending: std::sync::Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
        child: std::sync::Arc<Mutex<Option<Child>>>,
    },
    Http {
        http: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        session_id: std::sync::Arc<AsyncMutex<Option<String>>>,
    },
}

pub struct McpClient {
    transport: Transport,
    next_id: AtomicU64,
    server_name: String,
}

impl McpClient {
    /// Connect to the server and perform the initialize handshake. A config
    /// with a `url` uses the streamable-HTTP transport; otherwise the server
    /// command is spawned with stdio transport.
    pub async fn spawn(server_name: &str, cfg: &McpServerConfig) -> Result<std::sync::Arc<Self>> {
        let http_url = cfg.url.as_deref().filter(|u| !u.is_empty());
        if http_url.is_some() && cfg.kind == "sse" {
            return Err(Error::Config(format!(
                "mcp server `{server_name}`: the legacy SSE transport is not supported; \
                 use a streamable-http `url` without `type`"
            )));
        }
        if http_url.is_none() && cfg.command.is_empty() {
            return Err(Error::Config(format!(
                "mcp server `{server_name}`: stdio transport requires a `command`"
            )));
        }
        let transport = if let Some(url) = http_url {
            Transport::Http {
                // Per-request timeout so even unwrapped notifications
                // (initialized) cannot hang spawn forever.
                http: reqwest::Client::builder()
                    .timeout(CALL_TIMEOUT)
                    .build()
                    .map_err(|e| Error::Config(format!("mcp server `{server_name}`: http client: {e}")))?,
                url: expand_placeholders(url),
                headers: cfg
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), expand_placeholders(v)))
                    .collect(),
                session_id: std::sync::Arc::new(AsyncMutex::new(None)),
            }
        } else {
            let mut command = Command::new(expand_placeholders(&cfg.command));
            let expanded_args: Vec<String> = cfg.args.iter().map(|a| expand_placeholders(a)).collect();
            command.args(&expanded_args);
            for (k, v) in &cfg.env {
                command.env(k, expand_placeholders(v));
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
            Transport::Stdio {
                writer: std::sync::Arc::new(AsyncMutex::new(stdin)),
                pending,
                child: std::sync::Arc::new(Mutex::new(Some(child))),
            }
        };

        let client = std::sync::Arc::new(McpClient {
            transport,
            next_id: AtomicU64::new(1),
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
        self.request("initialize", self.initialize_params()).await
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
                prompt_guidelines: parse_prompt_guidelines(t.get("promptGuidelines")),
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
        let req = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        match &self.transport {
            Transport::Stdio { writer, pending, .. } => {
                let (otx, orx) = oneshot::channel();
                pending.lock().expect("pending lock").insert(id, otx);
                let line = serde_json::to_string(&req).map_err(|e| Error::Tool {
                    tool: "MCP".into(),
                    message: format!("encode: {e}"),
                })?;
                {
                    let mut w = writer.lock().await;
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
                        pending.lock().expect("pending lock").remove(&id);
                        Err(Error::Tool {
                            tool: "MCP".into(),
                            message: format!("timeout calling `{method}` on `{}`", self.server_name),
                        })
                    }
                }
            }
            Transport::Http { .. } => {
                let roundtrip = self.http_roundtrip(&req, method, id, /* allow_reinit */ true);
                let v = match tokio::time::timeout(CALL_TIMEOUT, roundtrip).await {
                    Ok(r) => r?,
                    Err(_) => {
                        return Err(Error::Tool {
                            tool: "MCP".into(),
                            message: format!(
                                "timeout calling `{method}` on `{}`",
                                self.server_name
                            ),
                        })
                    }
                };
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
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let req = json!({"jsonrpc":"2.0","method":method,"params":params});
        match &self.transport {
            Transport::Stdio { writer, .. } => {
                let line = serde_json::to_string(&req).map_err(|e| Error::Tool {
                    tool: "MCP".into(),
                    message: format!("encode: {e}"),
                })?;
                let mut w = writer.lock().await;
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
            // The `initialized` notification is advisory in streamable HTTP
            // (servers reply 202 with no body); failures are non-fatal.
            Transport::Http { .. } => {
                let _ = self.http_post(&req, None).await;
                Ok(())
            }
        }
    }

    fn initialize_params(&self) -> Value {
        let protocol_version = match &self.transport {
            Transport::Http { .. } => MCP_HTTP_PROTOCOL_VERSION,
            Transport::Stdio { .. } => "2024-11-05",
        };
        json!({
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": {"name": "nonoclaw", "version": "0.1.0"}
        })
    }

    /// POST one JSON-RPC message and return the raw reply. `expect_id` is set
    /// for requests; on an SSE response only the event whose JSON-RPC `id`
    /// matches is returned, so an otherwise-open stream cannot hang the call.
    async fn http_post(&self, req: &Value, expect_id: Option<u64>) -> Result<HttpReply> {
        let Transport::Http {
            http,
            url,
            headers,
            session_id,
        } = &self.transport
        else {
            return Err(Error::Tool {
                tool: "MCP".into(),
                message: "http_post on non-http transport".into(),
            });
        };
        let mut builder = http
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", MCP_HTTP_PROTOCOL_VERSION);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        if let Some(sid) = session_id.lock().await.clone() {
            builder = builder.header("Mcp-Session-Id", sid);
        }
        let mut resp = builder.json(req).send().await.map_err(|e| Error::Tool {
            tool: "MCP".into(),
            message: format!("http `{}`: {e}", self.server_name),
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Tool {
                tool: "MCP".into(),
                message: format!(
                    "http `{}`: status {} {body}",
                    self.server_name,
                    status.as_u16()
                ),
            });
        }
        let session_header = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        // 202 Accepted (notifications) has no body.
        if status == reqwest::StatusCode::NO_CONTENT || content_type.is_empty() {
            return Ok(HttpReply {
                message: None,
                session_id: session_header,
            });
        }
        if content_type.contains("text/event-stream") {
            let mut matcher = SseMatcher::new(expect_id);
            loop {
                match resp.chunk().await {
                    Ok(Some(bytes)) => {
                        matcher.feed(&bytes);
                        if let Some(v) = matcher.take() {
                            return Ok(HttpReply {
                                message: Some(v),
                                session_id: session_header,
                            });
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        return Err(Error::Tool {
                            tool: "MCP".into(),
                            message: format!("read sse `{}`: {e}", self.server_name),
                        })
                    }
                }
            }
            matcher.finish();
            return Ok(HttpReply {
                message: matcher.take(),
                session_id: session_header,
            });
        }
        // Single JSON-RPC response in one body.
        let v: Value = resp.json().await.map_err(|e| Error::Tool {
            tool: "MCP".into(),
            message: format!("decode json `{}`: {e}", self.server_name),
        })?;
        Ok(HttpReply {
            message: Some(v),
            session_id: session_header,
        })
    }

    /// One HTTP round trip with a single retry after re-initialization when
    /// the server reports the session expired (404/410) per the MCP spec.
    async fn http_roundtrip(
        &self,
        req: &Value,
        method: &str,
        id: u64,
        allow_reinit: bool,
    ) -> Result<Value> {
        let Transport::Http { session_id, .. } = &self.transport else {
            return Err(Error::Tool {
                tool: "MCP".into(),
                message: "http_roundtrip on non-http transport".into(),
            });
        };
        let reply = self.http_post(req, Some(id)).await;
        let session_expired = match &reply {
            Err(Error::Tool { message, .. }) => {
                let had_session = session_id.lock().await.is_some();
                had_session && (message.contains("status 404") || message.contains("status 410"))
            }
            _ => false,
        };
        let reply = if session_expired && allow_reinit {
            *session_id.lock().await = None;
            let init_id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let init_req = json!({
                "jsonrpc":"2.0",
                "id": init_id,
                "method":"initialize",
                "params": self.initialize_params()
            });
            let init = tokio::time::timeout(
                INIT_TIMEOUT,
                self.http_post(&init_req, Some(init_id)),
            )
            .await;
            match init {
                Ok(Ok(r)) => {
                    if let Some(sid) = r.session_id {
                        *session_id.lock().await = Some(sid);
                    }
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(Error::Tool {
                        tool: "MCP".into(),
                        message: format!(
                            "timeout re-initializing `{}` after session expiry",
                            self.server_name
                        ),
                    })
                }
            }
            self.http_post(req, Some(id)).await?
        } else {
            reply?
        };
        if let Some(sid) = reply.session_id {
            *session_id.lock().await = Some(sid);
        }
        let Some(message) = reply.message else {
            return Err(Error::Tool {
                tool: "MCP".into(),
                message: format!(
                    "server `{}` returned no response body for `{method}`",
                    self.server_name
                ),
            });
        };
        Ok(message)
    }
}

/// Raw reply of one HTTP transport round trip.
struct HttpReply {
    message: Option<Value>,
    session_id: Option<String>,
}

/// Incremental SSE consumer: feed body chunks, take the first JSON-RPC
/// message whose `id` matches (`None` accepts the first message at all).
struct SseMatcher {
    expect_id: Option<u64>,
    buf: String,
    matched: Option<Value>,
}

impl SseMatcher {
    fn new(expect_id: Option<u64>) -> Self {
        Self {
            expect_id,
            buf: String::new(),
            matched: None,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.matched.is_some() {
            return;
        }
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        // SSE lines may end with CRLF; strip CR so the blank-line delimiter
        // is always a plain "\n\n" (raw CR cannot appear inside JSON data).
        if self.buf.contains('\r') {
            self.buf = self.buf.replace('\r', "");
        }
        while self.matched.is_none() {
            let Some(pos) = self.buf.find("\n\n") else {
                break;
            };
            let event: String = self.buf.drain(..pos + 2).collect();
            self.try_match_event(&event);
        }
    }

    /// Flush a trailing event that was not followed by a blank line.
    fn finish(&mut self) {
        if self.matched.is_some() || self.buf.is_empty() {
            return;
        }
        let event = std::mem::take(&mut self.buf);
        self.try_match_event(&event);
    }

    fn try_match_event(&mut self, event: &str) {
        let data: Vec<&str> = event
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.trim_start())
            .collect();
        if data.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&data.join("\n")) else {
            return;
        };
        let id_matches = match self.expect_id {
            Some(want) => v.get("id").and_then(|i| i.as_u64()) == Some(want),
            None => true,
        };
        if id_matches {
            self.matched = Some(v);
        }
    }

    fn take(&mut self) -> Option<Value> {
        self.matched.take()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Transport::Stdio { child, .. } = &self.transport {
            if let Some(mut c) = child.lock().expect("child lock").take() {
                let _ = c.start_kill();
                let _ = c.try_wait();
            }
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
    /// Behavioural guidelines the server wants surfaced in the system
    /// prompt (T8.3). Parsed from the tool definition's optional
    /// `promptGuidelines` field: a string, or an array of strings.
    pub prompt_guidelines: Vec<String>,
}

/// Parse the optional `promptGuidelines` field of an MCP tool definition.
/// Accepts a single string or an array of strings; anything else is ignored
/// (the MCP spec does not know this field — it is a NonoClaw extension, so
/// tolerance beats strictness).
fn parse_prompt_guidelines(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// A [`Tool`] wrapper around one MCP-provided tool.
pub struct McpTool {
    full_name: String,
    raw_name: String,
    description: String,
    input_schema: Value,
    prompt_guidelines: Vec<String>,
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
            prompt_guidelines: def.prompt_guidelines,
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
    /// Server-declared guidelines (T8.3). The trait demands `&[&str]` while
    /// our strings arrive dynamically from the server's tools/list response;
    /// leaking them is bounded (once per tool definition per registration)
    /// and the registry lives for the whole process anyway.
    fn prompt_guidelines(&self) -> &[&str] {
        if self.prompt_guidelines.is_empty() {
            return &[];
        }
        Box::leak(
            self.prompt_guidelines
                .iter()
                .map(|g| g.as_str())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
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
///
/// The expensive part (child process spawn + `initialize` handshake +
/// `tools/list`) runs concurrently across all servers — npx/python startup
/// dominates ACP/web boot time. Registry mutations stay sequential because
/// `ToolRegistry::register` takes `&mut self`.
pub async fn register(registry: &mut ToolRegistry, configs: &[(String, McpServerConfig)]) {
    let results = futures::future::join_all(configs.iter().map(|(name, cfg)| {
        let name = name.clone();
        let cfg = cfg.clone();
        async move {
            let source = format!("mcp-config:{name}");
            let descriptor = ExtensionDescriptor::new(
                ExtensionKind::Mcp,
                name.clone(),
                source.clone(),
                ExtensionSourceKind::Explicit,
                100,
            );
            let outcome = match McpClient::spawn(&name, &cfg).await {
                Ok(client) => match client.list_tools().await {
                    Ok(defs) => Ok(defs
                        .into_iter()
                        .map(|d| (name.clone(), d, std::sync::Arc::clone(&client)))
                        .collect::<Vec<_>>()),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            (name, source, descriptor, outcome)
        }
    }))
    .await;

    for (name, source, mut descriptor, outcome) in results {
        match outcome {
            Ok(tools) => {
                let n = tools.len();
                for (name, d, client) in tools {
                    registry.register(std::sync::Arc::new(McpTool::new(&name, d, client)));
                }
                descriptor.detail = Some(format!("connected; {n} tool(s)"));
                registry.add_extension_descriptor(descriptor);
                tracing::info!("MCP server `{name}`: registered {n} tool(s)");
            }
            Err(error) => {
                descriptor.status = ExtensionStatus::Failed;
                descriptor.detail = Some(error.to_string());
                registry.add_extension_descriptor(descriptor);
                registry.add_extension_diagnostic(mcp_failure(&name, &source, &error));
                tracing::warn!("MCP server `{name}` failed: {error}");
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

    #[test]
    fn prompt_guidelines_parse_string_or_array() {
        // Array form.
        let v = serde_json::json!(["use foo before bar", ""]);
        let g = parse_prompt_guidelines(Some(&v));
        assert_eq!(g, vec!["use foo before bar".to_string()]);
        // Single-string form.
        let v = serde_json::json!("always pass limit");
        assert_eq!(
            parse_prompt_guidelines(Some(&v)),
            vec!["always pass limit".to_string()]
        );
        // Absent / wrong type / empty → none.
        assert!(parse_prompt_guidelines(None).is_empty());
        assert!(parse_prompt_guidelines(Some(&serde_json::json!(42))).is_empty());
        assert!(parse_prompt_guidelines(Some(&serde_json::json!(""))).is_empty());
    }

    #[test]
    fn expands_nonoclaw_home_placeholder() {
        std::env::set_var("NONOCLAW_HOME", "C:\\Portable\\NonoClaw\\.nonoclaw");
        assert_eq!(
            expand_placeholders("${NONOCLAW_HOME}/mcp-proxies/zhihuiya-proxy.mjs"),
            "C:\\Portable\\NonoClaw\\.nonoclaw/mcp-proxies/zhihuiya-proxy.mjs"
        );
        // Unknown placeholders pass through untouched.
        assert_eq!(expand_placeholders("${WORKSPACE}/x"), "${WORKSPACE}/x");
    }

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
                url: None,
                headers: HashMap::new(),
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

    #[test]
    fn config_deserializes_stdio_and_http_shapes() {
        // Legacy stdio shape: no type / url / headers.
        let stdio: McpServerConfig = serde_json::from_str(
            r#"{"command": "npx", "args": ["-y", "some-server"]}"#,
        )
        .expect("stdio shape");
        assert_eq!(stdio.kind, "stdio");
        assert_eq!(stdio.command, "npx");
        assert!(stdio.url.is_none());

        // Streamable-http shape: url selects the transport, headers optional.
        let http: McpServerConfig = serde_json::from_str(
            r#"{"type": "http", "url": "https://mcp.example.com/mcp", "headers": {"Authorization": "Bearer t"}}"#,
        )
        .expect("http shape");
        assert_eq!(http.kind, "http");
        assert_eq!(http.url.as_deref(), Some("https://mcp.example.com/mcp"));
        assert_eq!(http.headers.get("Authorization").map(String::as_str), Some("Bearer t"));
        assert_eq!(http.command, ""); // url-only config needs no command

        // url alone (no type) also selects http.
        let url_only: McpServerConfig =
            serde_json::from_str(r#"{"url": "https://mcp.example.com/mcp"}"#).expect("url-only");
        assert!(url_only.url.is_some());
    }

    #[test]
    fn sse_matcher_extracts_matching_id_and_multiline_data() {
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"a\":1}}\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\n",
            "data:  \"id\":100,\"result\":[2]}\n",
            "\n",
        );
        // Matching id in the last event, terminated by a blank line: matched
        // during feed, so a still-streaming body never delays earlier ids.
        let mut m = SseMatcher::new(Some(100));
        m.feed(body.as_bytes());
        let got = m.take().expect("last event matched during feed");
        assert_eq!(got["result"][0], 2);

        // The same body without the trailing blank line: only `finish()`
        // flushes the dangling event.
        let unterminated = body.trim_end_matches('\n');
        let mut m = SseMatcher::new(Some(100));
        m.feed(unterminated.as_bytes());
        assert!(m.take().is_none(), "no blank line yet: event is dangling");
        m.finish();
        let got = m.take().expect("flushed dangling event");
        assert_eq!(got["result"][0], 2);

        // Matching id in the middle: extraction stops before later events,
        // so a never-closing stream cannot hang the call.
        let mut m = SseMatcher::new(Some(99));
        m.feed(body.as_bytes());
        let got = m.take().expect("first event matches id 99");
        assert_eq!(got["result"]["a"], 1);

        // CRLF framing: blank-line delimiter still splits events during feed.
        let crlf = body.replace('\n', "\r\n");
        let mut m = SseMatcher::new(Some(99));
        m.feed(crlf.as_bytes());
        let got = m.take().expect("CRLF event matched during feed");
        assert_eq!(got["result"]["a"], 1);

        // Notification-first body, any-message mode: skip non-matching ids
        // even without an expected id only when one is given.
        let mut m = SseMatcher::new(Some(7));
        m.feed(body.as_bytes());
        m.finish();
        assert!(m.take().is_none(), "no event carries id 7");
    }

    #[tokio::test]
    async fn http_transport_end_to_end_against_fake_server() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let session_expired = std::sync::Arc::new(AtomicBool::new(false));
        let expired_flag = std::sync::Arc::clone(&session_expired);
        let init_count = std::sync::Arc::new(AtomicUsize::new(0));
        let init_counter = std::sync::Arc::clone(&init_count);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let session: Option<String> = Some("sess-1".to_string());
            let mut rejected_once = false;
            loop {
                // Read one request (headers + body) per connection iteration.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    let n = sock.read(&mut byte).await.expect("read head");
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&byte);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head_str = String::from_utf8_lossy(&head).to_string();
                let content_length = head_str
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    sock.read_exact(&mut body).await.expect("read body");
                }
                let msg: Value = serde_json::from_slice(&body).expect("json body");
                let id = msg.get("id").and_then(|i| i.as_u64());
                let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

                let presented_sid = head_str.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("mcp-session-id").then(|| v.trim().to_string())
                });

                if id.is_none() {
                    // Notification: 202, no body.
                    sock.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .expect("202");
                    continue;
                }
                let Some(id) = id else { unreachable!() };

                // tools/call with an expired session: reject exactly once, so
                // the client must clear, re-initialize, and retry to succeed.
                if method == "tools/call"
                    && expired_flag.load(Ordering::SeqCst)
                    && presented_sid.is_some()
                    && !rejected_once
                {
                    rejected_once = true;
                    sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .expect("404");
                    continue;
                }

                let (response_body, is_sse) = match method {
                    "initialize" => {
                        init_counter.fetch_add(1, Ordering::SeqCst);
                        (
                            serde_json::json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": {"tools": {}},
                                    "serverInfo": {"name": "fake", "version": "0"}
                                }
                            })
                            .to_string(),
                            false,
                        )
                    }
                    "tools/list" => (
                        format!(
                            "event: message\r\ndata: {}\r\n\r\n",
                            serde_json::json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": {"tools": [{"name": "echo", "description": "Echo",
                                    "inputSchema": {"type": "object", "properties": {}}}]}
                            })
                        ),
                        true,
                    ),
                    "tools/call" => (
                        format!(
                            "event: message\r\ndata: {}\r\n\r\n",
                            serde_json::json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": {"content": [{"type": "text", "text": "hello from fake"}]}
                            })
                        ),
                        true,
                    ),
                    _ => (
                        serde_json::json!({
                            "jsonrpc": "2.0", "id": id, "error":
                            {"code": -32601, "message": "method not found"}
                        })
                        .to_string(),
                        false,
                    ),
                };
                let sid_header = format!("Mcp-Session-Id: {}\r\n", session.as_deref().unwrap_or(""));
                let head = if is_sse {
                    format!(
                        "HTTP/1.1 200 OK\r\n{sid_header}Content-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                        response_body.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\n{sid_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                        response_body.len()
                    )
                };
                sock.write_all(head.as_bytes()).await.expect("head");
                sock.write_all(response_body.as_bytes()).await.expect("body");
            }
        });

        let cfg = McpServerConfig {
            kind: "http".into(),
            command: String::new(),
            args: vec![],
            env: HashMap::new(),
            url: Some(format!("http://{addr}/mcp")),
            headers: HashMap::new(),
        };
        let client = McpClient::spawn("fake-http", &cfg).await.expect("connect");
        assert_eq!(init_count.load(Ordering::SeqCst), 1);
        let tools = client.list_tools().await.expect("tools/list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        // tools/call with a still-valid session.
        let result = client
            .call_tool("echo", json!({"text": "hi"}))
            .await
            .expect("tools/call");
        assert!(result.0.contains("hello from fake"));

        // Expire the session: next call gets 404 → re-init → retry succeeds.
        session_expired.store(true, Ordering::SeqCst);
        let result = client
            .call_tool("echo", json!({"text": "again"}))
            .await
            .expect("tools/call after re-init");
        assert!(result.0.contains("hello from fake"));
        assert_eq!(
            init_count.load(Ordering::SeqCst),
            2,
            "client must re-run the initialize handshake after session expiry"
        );
        server.abort();
    }
}
