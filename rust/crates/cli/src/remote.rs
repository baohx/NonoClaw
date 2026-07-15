//! Remote session transport. Mirrors the role of `src/remote/`
//! (`RemoteSessionManager`, `SessionsWebSocket`): expose the engine over the
//! network so a client can run a prompt and receive streamed events.
//!
//! `connect_inline` is consumed by the web frontend Phase 1.
#![allow(dead_code)]
//!
//! Transport is newline-delimited JSON over TCP (a WebSocket would also work;
//! TCP+JSON-lines keeps the dependency surface at zero). The server trusts its
//! localhost peer and runs each connection as an isolated, non-interactive agent
//! run (BypassPermissions) — suitable for local tooling, not open to the network.

use std::sync::Arc;

use anyhow::{Context, Result};
use nonoclaw_api::Client;
use nonoclaw_core::{MessageContent, PermissionMode};
use nonoclaw_engine::{EngineEvent, EngineOptions, FinalResult, QueryEngine};
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
    Event { event: EngineEvent },
    Done { result: FinalResult },
    Error { message: String },
}

/// Internal channel message from the engine callback to the socket writer.
enum ToWire {
    Event(EngineEvent),
    Done(Result<FinalResult, String>),
}

/// Run the server: accept connections forever, each handled in its own task.
pub async fn serve(addr: &str) -> Result<()> {
    let listener = TcpListener::bind(addr).await.context("bind failed")?;
    let bound = listener.local_addr()?;
    eprintln!("[serve] listening on {bound}");
    loop {
        let (stream, peer) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream).await {
                eprintln!("[serve] {peer}: {e}");
            }
        });
    }
}

async fn handle_conn(stream: TcpStream) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await
        .context("read request")?
        .context("client closed before sending a request")?;
    let req: RunRequest = serde_json::from_str(&line).context("parse RunRequest")?;

    let client = Client::from_env().context("build API client")?;
    let (registry, todos) = register_all();
    let mut options = EngineOptions {
        model: req
            .model
            .clone()
            .unwrap_or_else(|| EngineOptions::default().model),
        max_turns: req
            .max_turns
            .unwrap_or_else(|| EngineOptions::default().max_turns),
        permission_mode: PermissionMode::BypassPermissions,
        is_non_interactive: true,
        ..EngineOptions::default()
    };
    options.auto_compact = true;
    let mut engine = QueryEngine::new(Arc::new(client), Arc::new(registry), todos, options);
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

    let result = engine
        .run(MessageContent::from_text(&req.prompt), &cwd, |ev: &EngineEvent| {
            let _ = tx.try_send(ToWire::Event(ev.clone()));
        })
        .await;
    let _ = tx
        .send(ToWire::Done(result.map_err(|e| format!("{e}"))))
        .await;
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
                if v["event"]["kind"].as_str() == Some("text_delta") {
                    if let Some(t) = v["event"]["text"].as_str() {
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

/// Connect + stream events into a callback (used by `--bridge` TUI mode).
pub async fn connect_inline(
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
                if let Ok(e) = serde_json::from_value::<EngineEvent>(v["event"].clone()) {
                    on_event(&e);
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
