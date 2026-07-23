//! Remote session transport. Exposes the engine over newline-delimited JSON
//! so a client can run a prompt and receive streamed events.
//!
//! Transport is newline-delimited JSON over TCP (a WebSocket would also work;
//! TCP+JSON-lines keeps the dependency surface at zero). The server trusts its
//! localhost peer and runs each connection as an isolated, non-interactive agent
//! run (BypassPermissions) — suitable for local tooling, not open to the network.

use std::sync::Arc;

use anyhow::{Context, Result};
use nonoclaw_core::{MessageContent, PermissionMode};
#[cfg(test)]
use nonoclaw_engine::EngineEvent;
use nonoclaw_engine::{
    ClientPurpose, ConfigSource, EventEnvelope, FinalResult, QueryEngine, ResolvedConfig,
    RunConfigOverrides, RunController, RunTerminalStatus,
};
use nonoclaw_tools::register_all;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// A client's run request (first line on the connection).
#[derive(Debug, Serialize, Deserialize)]
pub struct RunRequest {
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
}

/// Messages the server streams back, one JSON object per line.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Wire {
    Event { event: EventEnvelope },
    Done { result: FinalResult },
    Error { message: String },
}

/// Internal channel message from the engine callback to the socket writer.
enum ToWire {
    Event(EventEnvelope),
    Done(Result<FinalResult, String>),
}

/// Run the server: accept connections forever, each handled in its own task.
pub async fn serve(addr: &str, resolved: Arc<ResolvedConfig>) -> Result<()> {
    let listener = TcpListener::bind(addr).await.context("bind failed")?;
    let bound = listener.local_addr()?;
    eprintln!("[serve] listening on {bound}");
    loop {
        let (stream, peer) = listener.accept().await?;
        let resolved = Arc::clone(&resolved);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, resolved).await {
                eprintln!("[serve] {peer}: {e}");
            }
        });
    }
}

async fn handle_conn(stream: TcpStream, resolved: Arc<ResolvedConfig>) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await
        .context("read request")?
        .context("client closed before sending a request")?;
    let req: RunRequest = serde_json::from_str(&line).context("parse RunRequest")?;

    let model = req
        .model
        .clone()
        .unwrap_or_else(|| resolved.active_model.value.clone());
    let client = resolved
        .client_for(ClientPurpose::Conversation, Some(&model))
        .context("build API client from resolved configuration")?;
    let (mut registry, todos) = register_all();
    let mcp_configs = resolved.mcp_configs();
    nonoclaw_tools::register_mcp(&mut registry, &mcp_configs).await;
    let tool_search = nonoclaw_tools::builtin::ToolSearchTool::new(registry.search_entries());
    registry.register(Arc::new(tool_search));
    let options = resolved
        .resolve_run(RunConfigOverrides {
            source: ConfigSource::RemoteRequest {
                field: "run options".into(),
            },
            model: Some(model),
            max_turns: req.max_turns,
            permission_mode: Some(PermissionMode::BypassPermissions),
            is_non_interactive: true,
            ..Default::default()
        })
        .options;
    let engine = QueryEngine::new(client, Arc::new(registry), todos, options);
    let cwd = std::env::current_dir().context("cwd")?;

    // Stream engine events to the socket via a channel + writer task.
    let (tx, mut rx) = mpsc::channel::<ToWire>(64);
    let mut writer = writer;
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let wire = match msg {
                ToWire::Event(e) => Wire::Event { event: e },
                ToWire::Done(Ok(r)) => Wire::Done { result: r },
                ToWire::Done(Err(m)) => Wire::Error { message: m },
            };
            let mut line = serde_json::to_string(&wire).unwrap_or_default();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let controller = RunController::for_engine(&engine, cwd);
    let event_tx = tx.clone();
    let completion = controller
        .start(
            engine,
            MessageContent::from_text(&req.prompt),
            move |sequenced| {
                let event_tx = event_tx.clone();
                async move {
                    let _ = event_tx.send(ToWire::Event(sequenced)).await;
                }
            },
        )
        .wait()
        .await;
    let terminal = match completion.terminal.status {
        RunTerminalStatus::Done => completion
            .terminal
            .result
            .ok_or_else(|| "run completed without a result".to_string()),
        RunTerminalStatus::Cancelled | RunTerminalStatus::Error => {
            Err(format!("{:?}", completion.terminal.reason))
        }
    };
    let _ = tx.send(ToWire::Done(terminal)).await;
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

/// Connect to a server, send `req`, and print streamed text + the final result.
pub async fn connect(addr: &str, req: &RunRequest) -> Result<()> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let (reader, mut writer) = stream.into_split();
    let req_line = serde_json::to_string(req)?;
    writer
        .write_all(format!("{req_line}\n").as_bytes())
        .await
        .context("send request")?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await.context("read stream")? {
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("event") => {
                let event = v["event"].get("event").unwrap_or(&v["event"]);
                if event["kind"].as_str() == Some("text_delta") {
                    if let Some(t) = event["text"].as_str() {
                        use std::io::Write;
                        let _ = out.write_all(t.as_bytes());
                        let _ = out.flush();
                    }
                }
            }
            Some("done") => {
                use std::io::Write;
                let _ = writeln!(out);
                if let Some(text) = v["result"]["text"].as_str() {
                    let _ = writeln!(out, "\n[done] {text}");
                }
                let usage = &v["result"]["usage"];
                let _ = writeln!(
                    out,
                    "[turns {}, in {}, out {}]",
                    v["result"]["turns"].as_u64().unwrap_or(0),
                    usage["input_tokens"].as_u64().unwrap_or(0),
                    usage["output_tokens"].as_u64().unwrap_or(0),
                );
                break;
            }
            Some("error") => {
                let msg = v["message"].as_str().unwrap_or("unknown error");
                anyhow::bail!("server error: {msg}");
            }
            _ => {}
        }
    }
    Ok(())
}

/// Test adapter that captures streamed remote events without writing stdout.
#[cfg(test)]
async fn connect_inline(
    addr: &str,
    req: &RunRequest,
    mut on_event: impl FnMut(&EngineEvent),
) -> Result<()> {
    let stream = TcpStream::connect(addr).await?;
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(format!("{}\n", serde_json::to_string(req)?).as_bytes())
        .await?;
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        match v.get("type").and_then(|t| t.as_str()) {
            Some("event") => {
                if let Ok(envelope) = serde_json::from_value::<EventEnvelope>(v["event"].clone()) {
                    on_event(&envelope.event);
                } else if let Ok(event) = serde_json::from_value::<EngineEvent>(v["event"].clone())
                {
                    on_event(&event);
                }
            }
            Some("done") => return Ok(()),
            Some("error") => {
                let msg = v["message"].as_str().unwrap_or("").to_string();
                anyhow::bail!("{msg}");
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Characterizes the remote client's JSONL request/event/done success path
    /// using a loopback transport fixture only. Feature Preservation Matrix: §2.2/§4.
    #[tokio::test]
    async fn remote_client_minimal_success_path() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let request: RunRequest =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request.prompt, "fixture prompt");
            assert_eq!(request.model.as_deref(), Some("fixture-model"));
            assert_eq!(request.max_turns, Some(1));

            let event = serde_json::json!({
                "type": "event",
                "event": {"kind": "text_delta", "text": "remote answer"}
            });
            let done = serde_json::json!({
                "type": "done",
                "result": {
                    "text": "remote answer",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 2,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0
                    },
                    "turns": 1,
                    "stop_reason": "end_turn"
                }
            });
            writer
                .write_all(format!("{event}\n{done}\n").as_bytes())
                .await
                .unwrap();
        });

        let request = RunRequest {
            prompt: "fixture prompt".into(),
            model: Some("fixture-model".into()),
            max_turns: Some(1),
        };
        let mut events = Vec::new();
        connect_inline(&addr.to_string(), &request, |event| {
            events.push(event.clone())
        })
        .await
        .unwrap();
        fixture.await.unwrap();

        assert!(matches!(
            events.as_slice(),
            [EngineEvent::TextDelta { text }] if text == "remote answer"
        ));
    }
}
