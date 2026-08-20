//! Agent Client Protocol (ACP) stdio server.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout so an ACP client (e.g. the Zed editor)
//! can drive NonoClaw as an agent. Implemented methods:
//!   * `initialize`
//!   * `session/new`
//!   * `session/prompt` (streams `session/update` notifications, then a result)
//!   * `session/cancel`
//!
//! The prompt turn runs in the background; the reader loop stays responsive so
//! `session/cancel` is honored while a turn is in flight. All stdout writes are
//! serialized through a single lock so JSON-RPC framing never interleaves.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use nonoclaw_core::{MessageContent, PermissionMode, RunEvent};
use nonoclaw_engine::{
    ClientPurpose, ConfigSource, QueryEngine, RunConfigOverrides, RunController, RunTerminalStatus,
    Session, SessionService, SkillsManager,
};
use nonoclaw_tools::{BackgroundTaskRegistry, TodoStore, ToolRegistry};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Shared state for the ACP server.
struct AcpState {
    cwd: PathBuf,
    resolved: Arc<nonoclaw_engine::ResolvedConfig>,
    registry: Arc<ToolRegistry>,
    todos: Arc<TodoStore>,
    skills_manager: Arc<RwLock<SkillsManager>>,
    background_registry: Arc<std::sync::Mutex<BackgroundTaskRegistry>>,
    session_service: SessionService,
    /// Session id → Session handle (created by `session/new`).
    sessions: Mutex<HashMap<String, Session>>,
    /// Session id → active run controller (cancellable by `session/cancel`).
    controllers: Mutex<HashMap<String, RunController>>,
}

/// A completed prompt turn waiting to be answered on the main loop.
struct PendingDone {
    id: Option<Value>,
    result: Value,
}

pub async fn serve_stdin(
    registry: Arc<ToolRegistry>,
    todos: Arc<TodoStore>,
    cwd: PathBuf,
    resolved: Arc<nonoclaw_engine::ResolvedConfig>,
    skills_manager: Arc<RwLock<SkillsManager>>,
    background_registry: Arc<std::sync::Mutex<BackgroundTaskRegistry>>,
) -> nonoclaw_core::Result<()> {
    let state = Arc::new(AcpState {
        cwd,
        resolved,
        registry,
        todos,
        skills_manager,
        background_registry,
        session_service: SessionService::new(),
        sessions: Mutex::new(HashMap::new()),
        controllers: Mutex::new(HashMap::new()),
    });
    serve_io(state, tokio::io::stdin(), tokio::io::stdout()).await
}

async fn serve_io<R, W>(
    state: Arc<AcpState>,
    reader: R,
    writer: W,
) -> nonoclaw_core::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let writer = Arc::new(Mutex::new(writer));
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<PendingDone>();

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        tokio::select! {
            biased;
            read = reader.read_line(&mut line) => {
                if read? == 0 {
                    break; // EOF — client gone
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    line.clear();
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                    line.clear();
                    continue;
                };
                line.clear();
                let id = msg.get("id").cloned();
                let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let params = msg.get("params").cloned().unwrap_or(Value::Null);

                match method {
                    "initialize" => {
                        respond(&writer, id, Ok(json!({
                            "protocolVersion": 1,
                            "agentCapabilities": {
                                "promptTurn": true,
                                "loadSession": true,
                            },
                            "authMethods": [],
                        }))).await;
                    }
                    "session/new" => {
                        let result = new_session(&state, &params).await;
                        respond(&writer, id, result).await;
                    }
                    "session/load" => {
                        let result = load_session(&state, &params).await;
                        respond(&writer, id, result).await;
                    }
                    "session/prompt" => {
                        // Run in the background so cancel stays responsive.
                        let state = Arc::clone(&state);
                        let writer = Arc::clone(&writer);
                        let tx = done_tx.clone();
                        tokio::spawn(async move {
                            let result = prompt(&state, &writer, &params).await;
                            let _ = tx.send(PendingDone { id, result });
                        });
                    }
                    "session/cancel" => {
                        cancel_session(&state, &params).await;
                        respond(&writer, id, Ok(json!({}))).await;
                    }
                    // Notifications carry no id — acknowledge nothing.
                    _ if id.is_none() => {}
                    _ => {
                        respond(&writer, id, Err(json!({
                            "code": -32601,
                            "message": format!("method not found: {method}"),
                        }))).await;
                    }
                }
            }
            Some(done) = done_rx.recv() => {
                respond(&writer, done.id, Ok(done.result)).await;
            }
        }
    }
    Ok(())
}

/// Write one JSON-RPC response (or error) as a single line.
async fn respond<W>(
    writer: &Arc<Mutex<W>>,
    id: Option<Value>,
    result: std::result::Result<Value, Value>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let Some(id) = id else {
        return;
    };
    let payload = match result {
        Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
        Err(e) => json!({"jsonrpc":"2.0","id":id,"error":e}),
    };
    write_line(writer, &payload).await;
}

async fn write_line<W>(writer: &Arc<Mutex<W>>, value: &Value)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut guard = writer.lock().await;
    let _ = guard.write_all(format!("{value}\n").as_bytes()).await;
    let _ = guard.flush().await;
}

/// Extract the joined text of ACP content blocks.
fn prompt_text(params: &Value) -> String {
    params
        .get("prompt")
        .and_then(|p| p.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

async fn new_session(
    state: &AcpState,
    params: &Value,
) -> std::result::Result<Value, Value> {
    let cwd = params
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.cwd.clone());
    let id = nonoclaw_engine::new_session_id();
    let model = state.resolved.active_model.value.clone();
    let session = state
        .session_service
        .create(&cwd, id.clone(), model)
        .map_err(|e| json!({"code": -32000, "message": format!("session create failed: {e}")}))?;
    state.sessions.lock().await.insert(id.clone(), session);
    Ok(json!({"sessionId": id, "cwd": cwd.to_string_lossy()}))
}

/// ACP `session/load`: resume a previously persisted session (the client —
/// e.g. Zed — keeps the thread→sessionId mapping and calls this when
/// reopening a thread). Mirrors `new_session`'s shape on success.
async fn load_session(
    state: &AcpState,
    params: &Value,
) -> std::result::Result<Value, Value> {
    let Some(session_id) = params.get("sessionId").and_then(|s| s.as_str()) else {
        return Err(json!({"code": -32602, "message": "missing sessionId"}));
    };
    let cwd = params
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.cwd.clone());
    // Reject ids that would escape the sessions directory.
    if session_id.contains('/') || session_id.contains("..") {
        return Err(json!({"code": -32602, "message": "invalid sessionId"}));
    }
    let Some(path) = nonoclaw_engine::session::session_path(&cwd, session_id) else {
        return Err(json!({"code": -32000, "message": "cannot determine session storage"}));
    };
    if !path.exists() {
        return Err(json!({
            "code": -32000,
            "message": format!("session not found: {session_id}"),
        }));
    }
    let model = state.resolved.active_model.value.clone();
    let session = state
        .session_service
        .resume(&cwd, session_id)
        .map_err(|e| json!({"code": -32000, "message": format!("session load failed: {e}")}))?;
    state
        .sessions
        .lock()
        .await
        .insert(session_id.to_string(), session);
    Ok(json!({"sessionId": session_id, "cwd": cwd.to_string_lossy(), "model": model}))
}

async fn cancel_session(state: &AcpState, params: &Value) {
    let Some(session_id) = params.get("sessionId").and_then(|s| s.as_str()) else {
        return;
    };
    if let Some(controller) = state.controllers.lock().await.remove(session_id) {
        controller.cancel("cancelled by ACP client");
    }
}

/// Run one prompt turn, streaming `session/update` notifications to `writer`.
async fn prompt<W>(
    state: &AcpState,
    writer: &Arc<Mutex<W>>,
    params: &Value,
) -> Value
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let Some(session_id) = params.get("sessionId").and_then(|s| s.as_str()).map(str::to_string)
    else {
        return json!({"code": -32602, "message": "missing sessionId"});
    };
    let session = {
        let sessions = state.sessions.lock().await;
        sessions.get(&session_id).cloned()
    };
    let Some(session) = session else {
        return json!({"code": -32001, "message": format!("unknown session: {session_id}")});
    };

    let text = prompt_text(params);
    if text.trim().is_empty() {
        return json!({"stopReason": "end_turn"});
    }

    let model = state.resolved.active_model.value.clone();
    let mut options = state
        .resolved
        .resolve_run(RunConfigOverrides {
            source: ConfigSource::WebRequest {
                field: "acp_prompt".into(),
            },
            model: Some(model.clone()),
            permission_mode: Some(PermissionMode::Auto),
            is_non_interactive: true,
            ..Default::default()
        })
        .options;
    options.skills_manager = Some(Arc::clone(&state.skills_manager));
    options.background_registry = Some(Arc::clone(&state.background_registry));

    let client = match state
        .resolved
        .client_for(ClientPurpose::Conversation, Some(&model))
    {
        Ok(client) => client,
        Err(e) => {
            return json!({"code": -32000, "message": format!("client build failed: {e}")});
        }
    };

    let snapshot = match session.snapshot().await {
        Ok(s) => s,
        Err(e) => {
            return json!({"code": -32000, "message": format!("session snapshot failed: {e}")});
        }
    };

    let engine = QueryEngine::with_session(
        client,
        Arc::clone(&state.registry),
        Arc::clone(&state.todos),
        options,
        session,
        snapshot,
    );
    let controller = RunController::for_engine(&engine, state.cwd.clone());
    state
        .controllers
        .lock()
        .await
        .insert(session_id.clone(), controller.clone());

    let writer_for_events = Arc::clone(writer);
    let sid = session_id.clone();
    let completion = controller
        .start(engine, MessageContent::from_text(&text), move |sequenced| {
            let writer = Arc::clone(&writer_for_events);
            let sid = sid.clone();
            async move {
                if let Some(update) = acp_update(&sequenced.event) {
                    let notification = json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": update,
                        },
                    });
                    write_line(&writer, &notification).await;
                }
            }
        })
        .wait()
        .await;

    state.controllers.lock().await.remove(&session_id);
    let stop_reason = match completion.terminal.status {
        RunTerminalStatus::Done => "end_turn",
        RunTerminalStatus::Cancelled => "cancelled",
        RunTerminalStatus::Error => "error",
    };
    json!({"stopReason": stop_reason})
}

/// Map a run event to an ACP `session/update` value, or `None` if the event is
/// not model-facing.
fn acp_update(event: &RunEvent) -> Option<Value> {
    match event {
        RunEvent::TextDelta { text } => Some(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text},
        })),
        RunEvent::ToolUseStart { id, name, input } => Some(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": name,
            "status": "in_progress",
            "rawInput": input,
        })),
        RunEvent::ToolResult { id, ok, preview } => Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if *ok { "completed" } else { "failed" },
            "content": [{"type": "text", "text": preview}],
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text_joins_text_blocks() {
        let params = json!({
            "sessionId": "s",
            "prompt": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "world"},
            ],
        });
        assert_eq!(prompt_text(&params), "hello\nworld");
    }

    #[test]
    fn prompt_text_ignores_non_text_blocks() {
        let params = json!({
            "prompt": [{"type": "resource_link", "uri": "file:///x"}],
        });
        assert_eq!(prompt_text(&params), "");
    }

    #[test]
    fn acp_update_maps_model_facing_events() {
        assert_eq!(
            acp_update(&RunEvent::TextDelta { text: "hi".into() })
                .unwrap()["sessionUpdate"],
            "agent_message_chunk"
        );
        let tool = acp_update(&RunEvent::ToolUseStart {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({"command": "ls"}),
        })
        .unwrap();
        assert_eq!(tool["sessionUpdate"], "tool_call");
        assert_eq!(tool["status"], "in_progress");
        assert_eq!(tool["toolCallId"], "t1");

        let result = acp_update(&RunEvent::ToolResult {
            id: "t1".into(),
            ok: true,
            preview: "out".into(),
        })
        .unwrap();
        assert_eq!(result["sessionUpdate"], "tool_call_update");
        assert_eq!(result["status"], "completed");
    }

    #[test]
    fn acp_update_ignores_technical_events() {
        assert!(acp_update(&RunEvent::Compacting).is_none());
    }

    /// Regression: the read loop must clear its line buffer between requests.
    /// Before the fix, every request after the first was appended to the
    /// previous line, failed JSON parsing, and was silently dropped — Zed
    /// would hang forever waiting for `session/new` after `initialize`.
    #[tokio::test]
    async fn serve_io_answers_multiple_requests_per_connection() {
        let cwd = std::env::temp_dir();
        let state = Arc::new(AcpState {
            cwd: cwd.clone(),
            resolved: Arc::new(nonoclaw_engine::load_resolved_config(&cwd, None, None)),
            registry: Arc::new(ToolRegistry::new()),
            todos: Arc::new(TodoStore::new()),
            skills_manager: Arc::new(RwLock::new(SkillsManager::new(&cwd))),
            background_registry: Arc::new(std::sync::Mutex::new(BackgroundTaskRegistry::new())),
            session_service: SessionService::new(),
            sessions: Mutex::new(HashMap::new()),
            controllers: Mutex::new(HashMap::new()),
        });

        let (a, b) = tokio::io::duplex(4096);
        let (server_r, server_w) = tokio::io::split(a);
        let (test_r, mut test_w) = tokio::io::split(b);
        let mut test_r = tokio::io::BufReader::new(test_r);
        let io_task = tokio::spawn(serve_io(state, server_r, server_w));

        test_w
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n\
                  {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"nope/method\",\"params\":{}}\n",
            )
            .await
            .unwrap();

        for expected_id in [1u64, 2u64] {
            let mut buf = String::new();
            tokio::time::timeout(std::time::Duration::from_secs(10), test_r.read_line(&mut buf))
                .await
                .expect("serve_io stopped answering after the first request")
                .unwrap();
            let parsed: Value = serde_json::from_str(&buf).unwrap();
            assert_eq!(parsed["id"], expected_id, "unexpected response: {buf}");
        }

        io_task.abort();
    }

    /// `session/load` must resume a persisted session and reject unknown /
    /// path-traversing ids. The persisted session lives under the cwd's
    /// project sessions dir, same store the Web UI and CLI share.
    #[tokio::test]
    async fn session_load_resumes_persisted_session_and_rejects_bad_ids() {
        let cwd = std::env::temp_dir().join("acp_load_test");
        std::fs::create_dir_all(&cwd).unwrap();
        let state = Arc::new(AcpState {
            cwd: cwd.clone(),
            resolved: Arc::new(nonoclaw_engine::load_resolved_config(&cwd, None, None)),
            registry: Arc::new(ToolRegistry::new()),
            todos: Arc::new(TodoStore::new()),
            skills_manager: Arc::new(RwLock::new(SkillsManager::new(&cwd))),
            background_registry: Arc::new(std::sync::Mutex::new(BackgroundTaskRegistry::new())),
            session_service: SessionService::new(),
            sessions: Mutex::new(HashMap::new()),
            controllers: Mutex::new(HashMap::new()),
        });

        // Seed a persisted session the client would reference. Persistence is
        // lazy (the writer actor flushes on the first mutation), so append a
        // message to force the file to land.
        let seeded = state
            .session_service
            .create(&cwd, "acp-seed-1", "model-x")
            .unwrap();
        let append = seeded
            .append(nonoclaw_core::Message::user(
                nonoclaw_core::MessageContent::from_text("seed"),
            ))
            .await
            .unwrap();
        assert!(append >= 1);
        let seed_path = nonoclaw_engine::session::session_path(&cwd, "acp-seed-1").unwrap();
        assert!(seed_path.exists(), "seeded session must be persisted");

        // Unknown id → error.
        let err = load_session(&state, &json!({"sessionId": "no-such-id"}))
            .await
            .unwrap_err();
        assert_eq!(err["code"], -32000);

        // Path traversal → rejected before touching the filesystem.
        let err = load_session(
            &state,
            &json!({"sessionId": "../../etc/passwd"}),
        )
        .await
        .unwrap_err();
        assert_eq!(err["code"], -32602);

        // Valid id → resumed and registered.
        let ok = load_session(&state, &json!({"sessionId": "acp-seed-1"}))
            .await
            .unwrap();
        assert_eq!(ok["sessionId"], "acp-seed-1");
        assert!(
            state.sessions.lock().await.contains_key("acp-seed-1"),
            "loaded session must be registered for later session/prompt calls"
        );
        drop(seeded);
    }
}
