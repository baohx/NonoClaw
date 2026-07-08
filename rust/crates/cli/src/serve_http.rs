//! HTTP + WebSocket server for the web frontend.
//!
//! Exposes the engine over a local HTTP server with a bidirectional WebSocket
//! protocol. The browser SPA connects via `/ws` and exchanges tagged JSON
//! messages. Permission / question prompts are resolved interactively via
//! oneshot channels bridged across the WebSocket.
//!
//! Each WebSocket connection owns one [`SessionHandle`] (id + on-disk file +
//! working messages). Sessions persist per-cwd to disk so a page refresh or
//! server restart can resume them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use axum::{
    extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::get,
    Router,
};
use axum::body::Body;
use axum::http::StatusCode;
use futures::{SinkExt, StreamExt};
use nonoclaw_api::Client;
use nonoclaw_core::{Message, PermissionDecision};
use nonoclaw_engine::{EngineEvent, EngineOptions, PermissionRequest, QueryEngine};
use nonoclaw_tools::tool::{QuestionRequest, QuestionResolver};
use nonoclaw_tools::{McpServerConfig, TodoStore, ToolRegistry};

use crate::project_info::{gather, ProjectInfo};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use uuid::Uuid;

// ── Type aliases ────────────────────────────────────────────────────────────

type Tx = Arc<Mutex<futures::stream::SplitSink<WebSocket, WsMessage>>>;
type PermissionMap = Mutex<HashMap<String, oneshot::Sender<PermissionDecision>>>;
type QuestionMap = Mutex<HashMap<String, oneshot::Sender<Option<String>>>>;

// ── Wire protocol ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Run {
        prompt: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        max_turns: Option<u32>,
    },
    Cancel,
    Clear,
    /// Start a fresh session for this connection.
    NewSession,
    /// Resume an existing session by id; server replies with Info + MessagesLoaded.
    ResumeSession { id: String },
    /// Manually trigger compaction of the active session.
    Compact,
    PermissionDecision {
        request_id: String,
        decision: String,
    },
    QuestionAnswer {
        request_id: String,
        #[serde(default)]
        answer: Option<String>,
    },
    /// Request the project file tree (rooted at cwd). Server replies FileTree.
    FileTree,
    /// Open a file/dir in the OS default handler — or `code` when force_code.
    /// Path must be relative to cwd (no traversal).
    OpenFile {
        path: String,
        #[serde(default)]
        force_code: bool,
    },
    /// Request a refresh of the full project context (tools/mcp/skills/.../git).
    ProjectInfoRefresh,
    /// Request the patch for a commit (Git pane → GitShow reply).
    GitShow { sha: String },
}

#[derive(Debug, Serialize, Clone)]
struct SessionInfoWire {
    id: String,
    started: Option<String>,
    message_count: usize,
    summary: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum ServerMsg {
    /// Streamed engine event (pre-serialised `EngineEvent` because it uses
    /// internal tagging and can't be flattened).
    Event { event: serde_json::Value },
    PermissionRequired {
        request_id: String,
        tool_name: String,
        message: String,
        input: serde_json::Value,
    },
    QuestionRequired {
        request_id: String,
        prompt: String,
        options: Vec<String>,
    },
    Done {
        text: String,
        usage: serde_json::Value,
        turns: u32,
        stop_reason: Option<String>,
    },
    Error { message: String },
    /// Emitted when the active session changes (after New/Resume). Carries the
    /// real on-disk session id.
    Info { model: String, session_id: String },
    /// Sent once right after the WS handshake. Lists resumable sessions for cwd.
    SessionList { sessions: Vec<SessionInfoWire> },
    /// Replays the full transcript after Resume (or [] for a fresh session) so
    /// the frontend can render the prior conversation.
    MessagesLoaded { messages: Vec<serde_json::Value> },
    /// The project file tree rooted at cwd (sent on connect + on FileTree req).
    /// `entries` is a flat pre-order list with explicit `depth` so the frontend
    /// can render indented rows without a second pass.
    FileTree { root: String, entries: Vec<FileEntry> },
    /// Full project context for the Insight rail + Git pane (sent on connect,
    /// on refresh, and after each run so git reflects post-run state).
    ProjectInfo { info: ProjectInfo },
    /// Patch + stat for a clicked commit (Git pane).
    GitShow { sha: String, output: String },
}

/// One node in the flattened file tree.
#[derive(Debug, Serialize, Clone)]
struct FileEntry {
    /// Path relative to cwd, using forward slashes.
    path: String,
    /// Final path segment (display name).
    name: String,
    is_dir: bool,
    /// Indentation depth (0 = direct children of cwd).
    depth: u32,
}

// ── Shared application state ────────────────────────────────────────────────

struct AppState {
    client: Arc<Client>,
    registry: Arc<ToolRegistry>,
    todos: Arc<TodoStore>,
    cwd: PathBuf,
    model: String,
    /// MCP server configs (name → config) — for the Insight panel.
    mcp_configs: Vec<(String, McpServerConfig)>,
    /// MCP server name → human-readable config source label.
    mcp_sources: HashMap<String, String>,
    /// Configured model context window (tokens), if any — surfaced in Insight.
    context_window: Option<usize>,
    /// Effective auto-compact threshold (tokens) — surfaced in Insight.
    compact_threshold: usize,
    pending_permissions: Arc<PermissionMap>,
    pending_questions: Arc<QuestionMap>,
}

// ── Per-connection session ──────────────────────────────────────────────────

/// A resolved session bound to one WebSocket connection: its id, on-disk file,
/// and the in-memory working message set (the source of truth for the next run).
struct SessionHandle {
    id: String,
    file: PathBuf,
    messages: Vec<Message>,
}

/// Validate a session id against path traversal / cross-project leakage.
/// Accepts UUID chars (hex + dash) only.
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Create a fresh session handle. Does NOT pre-create the file — the engine
/// writes the header idempotently on the first `run()`, so an abandoned "New
/// session" leaves no empty JSONL on disk.
fn create_new_session(state: &AppState) -> Option<SessionHandle> {
    let id = nonoclaw_engine::new_session_id();
    let file = nonoclaw_engine::session_path(&state.cwd, &id)?;
    Some(SessionHandle {
        id,
        file,
        messages: Vec::new(),
    })
}

/// Load an existing session by id, reconstructing its messages.
fn resume_session(state: &AppState, id: &str) -> Result<SessionHandle, String> {
    if !valid_session_id(id) {
        return Err(format!("invalid session id: {id}"));
    }
    let file = nonoclaw_engine::session_path(&state.cwd, id)
        .ok_or_else(|| "no session storage (set HOME or NONOCLAW_HOME)".to_string())?;
    if !file.exists() {
        return Err(format!("session {id} not found"));
    }
    let (_started, _summary, messages) = nonoclaw_engine::load_session(&file)
        .map_err(|e| format!("load failed: {e}"))?;
    Ok(SessionHandle {
        id: id.to_string(),
        file,
        messages,
    })
}

// ── Question resolver (bridged over WebSocket via oneshot) ──────────────────

struct WsQuestionResolver {
    request_id: String,
    pending: Arc<QuestionMap>,
    tx: Tx,
}

impl QuestionResolver for WsQuestionResolver {
    fn ask(
        &self,
        req: QuestionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + '_>> {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let request_id = self.request_id.clone();
        Box::pin(async move {
            let (otx, orx) = oneshot::channel();
            pending.lock().await.insert(request_id.clone(), otx);
            let msg = ServerMsg::QuestionRequired {
                request_id,
                prompt: req.prompt,
                options: req.options,
            };
            if let Ok(text) = serde_json::to_string(&msg) {
                let _ = tx.lock().await.send(WsMessage::Text(text)).await;
            }
            orx.await.unwrap_or_default()
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn serialize_event(ev: &EngineEvent) -> serde_json::Value {
    match serde_json::to_value(ev) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize EngineEvent");
            serde_json::json!({"kind": "text_delta", "text": format!("[serialize error: {e}]")})
        }
    }
}

async fn send_msg(tx: &Tx, msg: ServerMsg) {
    if let Ok(text) = serde_json::to_string(&msg) {
        if let Err(e) = tx.lock().await.send(WsMessage::Text(text)).await {
            tracing::warn!("ws send error: {e}");
        }
    }
}

fn make_permission_resolver(
    tx: Tx,
    pending: Arc<PermissionMap>,
) -> nonoclaw_engine::PermissionResolver {
    Arc::new(move |req: PermissionRequest| {
        let tx = tx.clone();
        let pending = Arc::clone(&pending);
        Box::pin(async move {
            let (otx, orx) = oneshot::channel();
            let request_id = Uuid::new_v4().to_string();
            pending.lock().await.insert(request_id.clone(), otx);
            let msg = ServerMsg::PermissionRequired {
                request_id,
                tool_name: req.tool_name,
                message: req.message,
                input: req.input,
            };
            if let Ok(text) = serde_json::to_string(&msg) {
                let _ = tx.lock().await.send(WsMessage::Text(text)).await;
            }
            match orx.await {
                Ok(decision) => decision,
                Err(_) => PermissionDecision::deny("request cancelled"),
            }
        })
    })
}

fn build_options(
    model: Option<String>,
    max_turns: Option<u32>,
    tx: Tx,
    pending_permissions: Arc<PermissionMap>,
) -> EngineOptions {
    let permission_resolver = make_permission_resolver(tx, pending_permissions);

    EngineOptions {
        model: model.unwrap_or_else(|| "claude-sonnet-4-5-20250929".into()),
        max_tokens: 8192,
        permission_mode: nonoclaw_core::PermissionMode::Default,
        allowed_tools: vec![],
        disallowed_tools: vec![],
        add_dirs: vec![],
        max_turns: max_turns.unwrap_or(10),
        append_system_prompt: None,
        thinking: None,
        is_non_interactive: false,
        permission_resolver: Some(permission_resolver),
        question_resolver: None, // set per-engine below
        auto_compact: true,
        // Fire compact early: many providers have ~1M context windows
        // (DeepSeek: 1,048,565). 80K keeps history manageable.
        compact_threshold_tokens: 80_000,
    }
}

fn list_sessions_wire(state: &AppState) -> Vec<SessionInfoWire> {
    nonoclaw_engine::list_sessions(&state.cwd)
        .unwrap_or_default()
        .into_iter()
        .map(|s| SessionInfoWire {
            id: s.id,
            started: s.started,
            message_count: s.message_count,
            summary: s.summary,
        })
        .collect()
}

// ── Public entry point ──────────────────────────────────────────────────────

/// Resolve the frontend dist directory.
///
/// Looks relative to the current working directory first, then fixed install
/// locations, so the server works from any directory after `./install.sh`.
fn frontend_dir(cwd: &Path) -> Option<PathBuf> {
    let exe_parent = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let mut candidates: Vec<PathBuf> = vec![
        // 1. cwd-relative (development: run from workspace root)
        cwd.join("frontend/dist"),
        // 2. cwd-relative from rust/ subdirectory
        cwd.join("../frontend/dist"),
    ];
    // 3. Fixed install locations (XDG data dir + ~/.nonoclaw).
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(data).join("nonoclaw/frontend/dist"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join(".local/share/nonoclaw/frontend/dist"));
        candidates.push(PathBuf::from(&home).join(".nonoclaw/frontend/dist"));
    }
    // 4. Binary-relative (dev: exe in rust/target/release, frontend at repo root).
    if let Some(d) = &exe_parent {
        candidates.push(d.join("../../../frontend/dist"));
    }
    for p in &candidates {
        if p.join("index.html").exists() {
            tracing::info!("Serving frontend from {}", p.display());
            return Some(p.clone());
        }
    }
    None
}

// ── File tree + opener (for the frontend left rail) ─────────────────────────

/// Directories never shown in the tree (heavy, generated, or VCS-internal).
const FILE_TREE_SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".cache",
    ".turbo",
    "__pycache__",
    ".venv",
    "venv",
    ".idea",
    ".gradle",
    ".dart_tool",
];
const FILE_TREE_MAX_DEPTH: u32 = 6;
const FILE_TREE_MAX_ENTRIES: usize = 5000;

/// Build a flat, pre-order file tree rooted at `root`.
fn build_file_tree(root: &Path) -> Vec<FileEntry> {
    let mut out = Vec::new();
    let mut count = 0usize;
    walk_dir(root, root, 0, &mut out, &mut count);
    out
}

fn walk_dir(root: &Path, dir: &Path, depth: u32, out: &mut Vec<FileEntry>, count: &mut usize) {
    if depth >= FILE_TREE_MAX_DEPTH || *count >= FILE_TREE_MAX_ENTRIES {
        return;
    }
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut items: Vec<std::fs::DirEntry> = read.filter_map(|e| e.ok()).collect();
    // Sort: directories first, then case-insensitive alphabetical.
    items.sort_by(|a, b| {
        let ad = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let bd = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
        match (ad, bd) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a
                .file_name()
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.file_name().to_string_lossy().to_lowercase()),
        }
    });
    for entry in items {
        if *count >= FILE_TREE_MAX_ENTRIES {
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        // Drop generated/VCS-heavy dirs everywhere; show dotfiles otherwise.
        if is_dir && FILE_TREE_SKIP_DIRS.iter().any(|s| *s == name) {
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        out.push(FileEntry {
            path: rel,
            name,
            is_dir,
            depth,
        });
        *count += 1;
        if is_dir {
            walk_dir(root, &entry.path(), depth + 1, out, count);
        }
    }
}

/// Resolve `rel` against cwd and confirm the result stays inside cwd.
/// Rejects absolute paths and any `..` component.
/// Resolve `rel` (absolute, or relative to `roots[0]` = cwd) and confirm it
/// stays inside one of the allowed canonicalized roots. Rejects `..` in
/// relative paths. Handles paths that don't exist yet (e.g. a fresh
/// `.nonoclaw/NONOCLAW.md`) by canonicalizing the longest existing ancestor
/// and re-appending the tail — so docs/settings can be created-on-open.
fn resolve_within(roots: &[PathBuf], rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if !p.is_absolute() && p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        roots.first()?.join(p)
    };
    // Walk up to the deepest existing ancestor, remembering the popped tail.
    let mut canon = joined.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match canon.canonicalize() {
            Ok(c) => {
                canon = c;
                break;
            }
            Err(_) => {
                if canon.parent().is_none() {
                    return None; // reached filesystem root without an existing ancestor
                }
                if let Some(name) = canon.file_name() {
                    tail.push(name.to_os_string());
                }
                canon = canon.parent()?.to_path_buf();
            }
        }
    }
    // The existing ancestor must sit under an allowed root.
    if !roots.iter().any(|r| canon.starts_with(r)) {
        return None;
    }
    // Re-append the (possibly non-existent) tail in original order.
    for name in tail.into_iter().rev() {
        canon.push(name);
    }
    Some(canon)
}

/// Allowed open roots: canonicalized cwd + (if it exists) the nonoclaw home.
fn open_roots(state: &AppState) -> Vec<PathBuf> {
    let mut roots = vec![state.cwd.canonicalize().unwrap_or_else(|_| state.cwd.clone())];
    if let Some(h) = crate::project_info::nonoclaw_home() {
        if let Ok(c) = h.canonicalize() {
            roots.push(c);
        }
    }
    roots
}

/// Open `path` with the platform's default associated program.
fn open_with_default(path: &Path) -> std::io::Result<()> {
    // Fire-and-forget: spawn the opener and don't wait for it.
    #[cfg(target_os = "macos")]
    {
        let _ = tokio::process::Command::new("open").arg(path).spawn()?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        let _ = tokio::process::Command::new("cmd")
            .args(["/C", "start", "", &path.to_string_lossy()])
            .spawn()?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match tokio::process::Command::new("xdg-open").arg(path).spawn() {
            Ok(_) => return Ok(()),
            Err(e) => {
                tracing::warn!("xdg-open unavailable ({e}); falling back to `code`");
                let _ = tokio::process::Command::new("code").arg(path).spawn()?;
                return Ok(());
            }
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no opener configured for this platform",
        ))
    }
}

pub async fn serve(
    addr: &str,
    client: Arc<Client>,
    registry: Arc<ToolRegistry>,
    todos: Arc<TodoStore>,
    cwd: PathBuf,
    model: String,
    mcp_configs: Vec<(String, McpServerConfig)>,
    mcp_sources: HashMap<String, String>,
    context_window: Option<usize>,
    compact_threshold: usize,
) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        client,
        registry,
        todos,
        cwd: cwd.clone(),
        model,
        mcp_configs,
        mcp_sources,
        context_window,
        compact_threshold,
        pending_permissions: Arc::new(Mutex::new(HashMap::new())),
        pending_questions: Arc::new(Mutex::new(HashMap::new())),
    });

    // Always register the WebSocket route.
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state);

    // Optionally serve the built frontend from frontend/dist/.
    let app = if let Some(fe_dir) = frontend_dir(&cwd) {
        let index_path = fe_dir.join("index.html");
        app.route(
            "/",
            axum::routing::get(|| async move {
                match tokio::fs::read(&index_path).await {
                    Ok(content) => axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/html; charset=utf-8")
                        .body(Body::from(content))
                        .unwrap(),
                    Err(_) => axum::response::Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::from("index.html not found"))
                        .unwrap(),
                }
            }),
        )
        .nest_service("/assets", ServeDir::new(fe_dir.join("assets")))
    } else {
        tracing::info!("No frontend/dist found; use Vite dev server for UI");
        app
    };

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("NonoClaw web UI listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── WebSocket handler ───────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(ws: WebSocket, state: Arc<AppState>) {
    let (tx, mut rx) = ws.split();
    let tx: Tx = Arc::new(Mutex::new(tx));
    let mut cancel: Option<CancellationToken> = None;

    // Per-connection session. None until chosen; the default-on-connect flow
    // below creates a fresh session immediately.
    let session: Arc<Mutex<Option<SessionHandle>>> = Arc::new(Mutex::new(None));

    // Connect handshake: send the resumable session list, then default to a
    // fresh session so the user can start typing immediately.
    {
        send_msg(
            &tx,
            ServerMsg::SessionList {
                sessions: list_sessions_wire(&state),
            },
        )
        .await;
        let new = create_new_session(&state);
        if let Some(h) = &new {
            send_msg(&tx, ServerMsg::MessagesLoaded { messages: vec![] }).await;
            send_msg(
                &tx,
                ServerMsg::Info {
                    model: state.model.clone(),
                    session_id: h.id.clone(),
                },
            )
            .await;
        } else {
            send_msg(
                &tx,
                ServerMsg::Error {
                    message: "no session storage (set HOME or NONOCLAW_HOME)".into(),
                },
            )
            .await;
        }
        *session.lock().await = new;

        // Send the project file tree so the frontend can render the left rail.
        send_msg(
            &tx,
            ServerMsg::FileTree {
                root: state.cwd.to_string_lossy().to_string(),
                entries: build_file_tree(&state.cwd),
            },
        )
        .await;

        // Send the full project context for the Insight rail + Git pane.
        let info = gather(
            &state.cwd,
            &state.model,
            &state.registry,
            &state.mcp_configs,
            &state.mcp_sources,
            state.context_window,
            state.compact_threshold,
        )
        .await;
        send_msg(&tx, ServerMsg::ProjectInfo { info }).await;
    }

    while let Some(Ok(msg)) = rx.next().await {
        let text = match &msg {
            WsMessage::Text(t) => t.clone(),
            WsMessage::Close(_) => break,
            _ => continue,
        };

        let parsed: ClientMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                send_msg(
                    &tx,
                    ServerMsg::Error {
                        message: format!("invalid message: {e}"),
                    },
                )
                .await;
                continue;
            }
        };

        match parsed {
            // ── New / Resume session ────────────────────────────────────────
            ClientMsg::NewSession => {
                let h = create_new_session(&state);
                match h {
                    Some(h) => {
                        send_msg(&tx, ServerMsg::MessagesLoaded { messages: vec![] }).await;
                        let sid = h.id.clone();
                        *session.lock().await = Some(h);
                        send_msg(
                            &tx,
                            ServerMsg::Info {
                                model: state.model.clone(),
                                session_id: sid,
                            },
                        )
                        .await;
                        // Refresh the list (the previously active session may
                        // now appear if it had content).
                        send_msg(
                            &tx,
                            ServerMsg::SessionList {
                                sessions: list_sessions_wire(&state),
                            },
                        )
                        .await;
                    }
                    None => {
                        send_msg(
                            &tx,
                            ServerMsg::Error {
                                message: "no session storage".into(),
                            },
                        )
                        .await;
                    }
                }
            }
            ClientMsg::ResumeSession { id } => match resume_session(&state, &id) {
                Ok(h) => {
                    let vals: Vec<serde_json::Value> =
                        h.messages.iter().filter_map(|m| serde_json::to_value(m).ok()).collect();
                    let sid = h.id.clone();
                    *session.lock().await = Some(h);
                    send_msg(&tx, ServerMsg::MessagesLoaded { messages: vals }).await;
                    send_msg(
                        &tx,
                        ServerMsg::Info {
                            model: state.model.clone(),
                            session_id: sid,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    send_msg(&tx, ServerMsg::Error { message: e }).await;
                }
            },

            // ── File tree + open-file (frontend left rail) ──────────────────
            ClientMsg::FileTree => {
                send_msg(
                    &tx,
                    ServerMsg::FileTree {
                        root: state.cwd.to_string_lossy().to_string(),
                        entries: build_file_tree(&state.cwd),
                    },
                )
                .await;
            }
            ClientMsg::ProjectInfoRefresh => {
                let info = gather(
                    &state.cwd,
                    &state.model,
                    &state.registry,
                    &state.mcp_configs,
                    &state.mcp_sources,
                    state.context_window,
                    state.compact_threshold,
                )
                .await;
                send_msg(&tx, ServerMsg::ProjectInfo { info }).await;
            }
            ClientMsg::GitShow { sha } => {
                match crate::project_info::git_show(&state.cwd, &sha).await {
                    Some(output) => {
                        send_msg(&tx, ServerMsg::GitShow { sha, output }).await;
                    }
                    None => {
                        send_msg(
                            &tx,
                            ServerMsg::Error {
                                message: format!("invalid or unknown commit: {sha}"),
                            },
                        )
                        .await;
                    }
                }
            }
            ClientMsg::OpenFile { path, force_code } => {
                let roots = open_roots(&state);
                match resolve_within(&roots, &path) {
                    Some(full) => {
                        // Clicking a doc that doesn't exist yet (e.g. a fresh
                        // NONOCLAW.md) is an intent to create+edit it: touch an
                        // empty file so the opener has something to open.
                        if !full.exists() {
                            if let Some(parent) = full.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = std::fs::File::create(&full);
                        }
                        let res = if force_code {
                            tokio::process::Command::new("code")
                                .arg(&full)
                                .spawn()
                                .map(|_| ())
                        } else {
                            open_with_default(&full)
                        };
                        if let Err(e) = res {
                            tracing::warn!(?full, "open failed: {e}");
                            send_msg(
                                &tx,
                                ServerMsg::Error {
                                    message: format!("open failed: {e}"),
                                },
                            )
                            .await;
                        }
                    }
                    None => {
                        send_msg(
                            &tx,
                            ServerMsg::Error {
                                message: format!("path outside cwd: {path}"),
                            },
                        )
                        .await;
                    }
                }
            }

            // ── Run ─────────────────────────────────────────────────────────
            ClientMsg::Run {
                prompt,
                model,
                max_turns,
            } => {
                tracing::info!(prompt = %prompt, model = ?model, "ws run request");
                // Require an active session.
                let prior = {
                    let g = session.lock().await;
                    match g.as_ref() {
                        Some(h) => h.messages.clone(),
                        None => {
                            send_msg(
                                &tx,
                                ServerMsg::Error {
                                    message: "no session selected".into(),
                                },
                            )
                            .await;
                            continue;
                        }
                    }
                };
                let session_file = {
                    let g = session.lock().await;
                    g.as_ref().map(|h| h.file.clone())
                };
                let session_id = {
                    let g = session.lock().await;
                    g.as_ref().map(|h| h.id.clone())
                };
                let (session_file, session_id) = match (session_file, session_id) {
                    (Some(f), Some(i)) => (f, i),
                    _ => {
                        send_msg(
                            &tx,
                            ServerMsg::Error {
                                message: "no session selected".into(),
                            },
                        )
                        .await;
                        continue;
                    }
                };

                // Cancel any in-progress run.
                if let Some(ref c) = cancel {
                    c.cancel();
                }
                let new_cancel = CancellationToken::new();
                cancel = Some(new_cancel.clone());

                let tx2 = tx.clone();
                let s = state.clone();
                let session2 = Arc::clone(&session);

                tokio::spawn(async move {
                    let request_id = Uuid::new_v4().to_string();
                    let model_used = model.unwrap_or_else(|| s.model.clone());
                    let mut options = build_options(
                        Some(model_used),
                        max_turns,
                        tx2.clone(),
                        Arc::clone(&s.pending_permissions),
                    );

                    // Question resolver (per-run to avoid oneshot key clashes).
                    let qr: Arc<dyn QuestionResolver> = Arc::new(WsQuestionResolver {
                        request_id: format!("{request_id}-q"),
                        pending: Arc::clone(&s.pending_questions),
                        tx: tx2.clone(),
                    });
                    options.question_resolver = Some(qr);

                    let mut engine = QueryEngine::with_session(
                        s.client.clone(),
                        s.registry.clone(),
                        s.todos.clone(),
                        options,
                        prior,
                        session_id,
                        Some(session_file),
                    );

                    // Order-preserving event relay: sync callback → channel →
                    // single consumer task → WebSocket.
                    let (ev_tx_chan, mut ev_rx_chan) =
                        tokio::sync::mpsc::unbounded_channel::<String>();
                    let tx_for_consumer = tx2.clone();
                    let consumer = tokio::spawn(async move {
                        while let Some(text) = ev_rx_chan.recv().await {
                            if let Err(e) =
                                tx_for_consumer.lock().await.send(WsMessage::Text(text)).await
                            {
                                tracing::warn!("ws send error: {e}");
                                break;
                            }
                        }
                    });

                    tracing::debug!(prompt = %prompt, "starting engine run");
                    let result = engine
                        .run(&prompt, &s.cwd, |ev| {
                            tracing::debug!(kind = ?ev, "engine event");
                            let msg = ServerMsg::Event {
                                event: serialize_event(ev),
                            };
                            if let Ok(text) = serde_json::to_string(&msg) {
                                let _ = ev_tx_chan.send(text);
                            }
                        })
                        .await;
                    drop(ev_tx_chan);
                    let _ = consumer.await;

                    // Persist accumulated messages back into the session handle.
                    {
                        let messages = engine.take_messages();
                        if let Some(h) = session2.lock().await.as_mut() {
                            h.messages = messages;
                        }
                    }

                    match result {
                        Ok(r) => {
                            tracing::info!(
                                turns = r.turns,
                                text_len = r.text.len(),
                                "engine run complete"
                            );
                            let msg = ServerMsg::Done {
                                text: r.text,
                                usage: serde_json::to_value(r.usage).unwrap_or_default(),
                                turns: r.turns,
                                stop_reason: r
                                    .stop_reason
                                    .as_ref()
                                    .map(|s| s.as_str().to_string()),
                            };
                            send_msg(&tx2, msg).await;

                            // Refresh project context: git status / files may
                            // have changed after the run.
                            let info = gather(
                                &s.cwd,
                                &s.model,
                                &s.registry,
                                &s.mcp_configs,
                                &s.mcp_sources,
                                s.context_window,
                                s.compact_threshold,
                            )
                            .await;
                            send_msg(&tx2, ServerMsg::ProjectInfo { info }).await;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "engine run failed");
                            if !new_cancel.is_cancelled() {
                                send_msg(&tx2, ServerMsg::Error { message: e.to_string() }).await;
                            }
                        }
                    }
                });
            }

            // ── Cancel ──────────────────────────────────────────────────────
            ClientMsg::Cancel => {
                if let Some(ref c) = cancel {
                    c.cancel();
                }
            }

            // ── Clear (in-memory only; on-disk transcript is the archive) ───
            ClientMsg::Clear => {
                if let Some(h) = session.lock().await.as_mut() {
                    h.messages.clear();
                }
                send_msg(&tx, ServerMsg::MessagesLoaded { messages: vec![] }).await;
            }

            // ── Manual /compact ─────────────────────────────────────────────
            ClientMsg::Compact => {
                // Pull the session handle out (we need ownership of messages).
                let handle_owned = session.lock().await.take();
                let mut handle_owned = match handle_owned {
                    Some(h) => h,
                    None => {
                        send_msg(
                            &tx,
                            ServerMsg::Error {
                                message: "no session".into(),
                            },
                        )
                        .await;
                        continue;
                    }
                };
                // Hint the UI that a summarization round-trip is in flight.
                send_msg(
                    &tx,
                    ServerMsg::Event {
                        event: serde_json::json!({"kind": "compacting"}),
                    },
                )
                .await;
                let opts = build_options(
                    None,
                    None,
                    tx.clone(),
                    Arc::clone(&state.pending_permissions),
                );
                let mut eng = QueryEngine::with_session(
                    state.client.clone(),
                    state.registry.clone(),
                    state.todos.clone(),
                    opts,
                    std::mem::take(&mut handle_owned.messages),
                    handle_owned.id.clone(),
                    Some(handle_owned.file.clone()),
                );
                match eng.compact_now().await {
                    Ok(Some((removed, kept))) => {
                        handle_owned.messages = eng.take_messages();
                        let ev = serde_json::json!({
                            "kind": "compacted",
                            "removed": removed,
                            "kept": kept,
                            "tokens_before": 0,
                            "tokens_after": 0,
                        });
                        *session.lock().await = Some(handle_owned);
                        send_msg(&tx, ServerMsg::Event { event: ev }).await;
                    }
                    Ok(None) => {
                        handle_owned.messages = eng.take_messages();
                        *session.lock().await = Some(handle_owned);
                        send_msg(
                            &tx,
                            ServerMsg::Info {
                                model: state.model.clone(),
                                session_id: String::new(),
                            },
                        )
                        .await;
                    }
                    Err(e) => {
                        handle_owned.messages = eng.take_messages();
                        *session.lock().await = Some(handle_owned);
                        send_msg(&tx, ServerMsg::Error { message: e.to_string() }).await;
                    }
                }
            }

            // ── Permission / question resolution ────────────────────────────
            ClientMsg::PermissionDecision {
                request_id,
                decision,
            } => {
                let sender = state.pending_permissions.lock().await.remove(&request_id);
                if let Some(sender) = sender {
                    let decision = match decision.as_str() {
                        "allow" => PermissionDecision::allow(),
                        _ => PermissionDecision::deny("user denied"),
                    };
                    let _ = sender.send(decision);
                }
            }
            ClientMsg::QuestionAnswer {
                request_id,
                answer,
            } => {
                let sender = state.pending_questions.lock().await.remove(&request_id);
                if let Some(sender) = sender {
                    let _ = sender.send(answer);
                }
            }
        }
    }
}
