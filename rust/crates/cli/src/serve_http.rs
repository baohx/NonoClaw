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
use std::sync::{Arc, RwLock};

use axum::{
    extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    extract::{Query, State},
    response::IntoResponse,
    routing::get,
    Router,
};
use axum::body::Body;
use axum::http::StatusCode;
use futures::{SinkExt, StreamExt};
use nonoclaw_api::Client;
use nonoclaw_core::{ContentBlock, ImageSource, Message, MessageContent, PermissionDecision};
use nonoclaw_engine::{substitute_arguments, EngineEvent, EngineOptions, ModelProfile, PermissionRequest, QueryEngine, SkillsManager};
use nonoclaw_tools::tool::{QuestionRequest, QuestionResolver};
use nonoclaw_tools::{McpServerConfig, TodoStore, ToolRegistry};

use crate::attachments;
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
        /// Skill body injected as additional system-prompt instructions (from
        /// `/skill-name` slash command).
        #[serde(default)]
        append_system_prompt: Option<String>,
        /// Raw argument string for skill invocation.
        #[serde(default)]
        arguments: Option<String>,
        /// Attached files whose content has been pre-extracted.
        #[serde(default)]
        attachments: Option<Vec<AttachmentRef>>,
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
    /// Switch the permission mode at runtime (default/auto/bypass/plan).
    SetPermissionMode { mode: String },
    /// Switch the active model at runtime (from the multi-model dropdown).
    SetModel { name: String },
}

/// Metadata for an uploaded + pre-extracted file attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachmentRef {
    id: String,
    filename: String,
    extracted_text: String,
    /// First few extracted images as base64 (for multimodal context).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<ImageRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImageRef {
    media_type: String,
    data: String,
}

/// Response from POST /api/upload.
#[derive(Debug, Serialize)]
struct UploadResponse {
    id: String,
    filename: String,
    extracted_text: String,
    image_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<ImageRef>>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct ModelInfo {
    name: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<usize>,
}

#[derive(Debug, Serialize, Clone)]
struct SessionInfoWire {
    id: String,
    started: Option<String>,
    message_count: usize,
    summary: String,
}

#[derive(Debug, Clone, Serialize)]
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
    /// real on-disk session id + the server auth token for QR-code remote access.
    Info {
        model: String,
        session_id: String,
        auth_token: String,
        available_models: Vec<ModelInfo>,
    },
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

// ── Shared session state ────────────────────────────────────────────────────

type SharedHandle = Arc<Mutex<Option<SessionHandle>>>;

/// A shared session: the handle + the set of connected Tx channels (used to
/// broadcast MessagesLoaded after a run so desktop ↔ mobile stay in sync).
struct SharedEntry {
    handle: SharedHandle,
    txs: Vec<Tx>,
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
    /// Auth token for remote (QR-code) mobile access. Empty string = no auth.
    auth_token: String,
    /// The public URL shown in the QR code, or None.
    public_url: Option<String>,
    /// Available model profiles from settings.json (may be empty → single-model).
    model_profiles: Vec<ModelProfile>,
    /// Currently active model name (switchable via SetModel client message + UI).
    active_model: Arc<Mutex<String>>,
    /// Session registry → session_id → (shared handle, connected Txs).
    session_registry: Arc<Mutex<HashMap<String, SharedEntry>>>,
    pending_permissions: Arc<PermissionMap>,
    pending_questions: Arc<QuestionMap>,
    /// Runtime-mutable permission mode (switchable via UI).
    permission_mode: Arc<Mutex<nonoclaw_core::PermissionMode>>,
    /// Skill manager: discovers, parses, and dynamically activates skills.
    skills_manager: Arc<RwLock<SkillsManager>>,
    /// Background task registry for run_in_background bash commands.
    background_registry: Arc<std::sync::Mutex<nonoclaw_tools::BackgroundTaskRegistry>>,
    /// Document processing model config (from settings).
    doc_model: Option<nonoclaw_engine::settings::DocModelConfig>,
    /// Optional model for compaction summarization (from settings).
    compact_model: Option<String>,
    /// Directory where uploaded attachments are stored.
    upload_dir: PathBuf,
}

// ── Per-connection session ──────────────────────────────────────────────────

/// A resolved session bound to one WebSocket connection: its id, on-disk file,
/// and the in-memory working message set (the source of truth for the next run).
#[derive(Clone)]
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

/// Build a user message from attachment content + the user's prompt.
///
/// Files that were OCR'd / processed have their text prepended.  When the
/// doc model returned extracted images (base64), they are injected as
/// `ContentBlock::Image` blocks so multimodal models can "see" the document
/// visually alongside the extracted text.
fn enrich_prompt_with_attachments(
    prompt: &str,
    attachments: &Option<Vec<AttachmentRef>>,
) -> MessageContent {
    let atts = match attachments {
        Some(a) if !a.is_empty() => a,
        _ => return MessageContent::from_text(prompt),
    };

    let mut blocks: Vec<ContentBlock> = Vec::new();

    blocks.push(ContentBlock::text(
        "The user has attached the following files. Their content has already been extracted and is shown below — you do NOT need to read or process these files. Just use the content directly.\n\n",
    ));

    for a in atts {
        blocks.push(ContentBlock::text(format!("## File: {}\n\n", a.filename)));

        // Inject extracted images first so the model can see them visually.
        for img in &a.images {
            if img.data.len() < 2_000_000 {
                // ~1.5 MB base64 → API size-safe
                blocks.push(ContentBlock::Image {
                    source: ImageSource {
                        kind: "base64".into(),
                        media_type: img.media_type.clone(),
                        data: img.data.clone(),
                    },
                });
                blocks.push(ContentBlock::text(format!(
                    "(extracted image: {})\n",
                    img.media_type
                )));
            }
        }

        let text = &a.extracted_text;
        let display = if text.chars().count() > attachments::MAX_INLINE_TEXT_CHARS {
            let truncated: String =
                text.chars().take(attachments::MAX_INLINE_TEXT_CHARS).collect();
            format!("{truncated}\n\n[... content truncated — the full file is available on disk]\n\n")
        } else {
            format!("{text}\n\n")
        };
        blocks.push(ContentBlock::text(display));
    }

    blocks.push(ContentBlock::text(format!(
        "---\n\n## User message\n\n{prompt}"
    )));

    MessageContent::from_blocks(blocks)
}

/// Re-read MCP configs from settings and .mcp.json on refresh, so newly
/// added servers appear in the Insight panel without restarting.
fn refresh_mcp_configs(
    cwd: &Path,
    existing: &[(String, McpServerConfig)],
) -> Vec<(String, McpServerConfig)> {
    let mut merged: std::collections::HashMap<String, McpServerConfig> = existing
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let settings = nonoclaw_engine::load_settings(cwd, None);
    if let Some(servers) = settings.mcp_servers {
        for (k, v) in servers {
            merged.entry(k).or_insert(v);
        }
    }
    if let Some(more) = nonoclaw_engine::settings::load_mcp_json(cwd) {
        for (k, v) in more {
            merged.entry(k).or_insert(v);
        }
    }
    let mut out: Vec<_> = merged.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

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

/// Broadcast the current session transcript to all peers EXCEPT `exclude`.
/// Used after Run / Clear so all connected devices stay in sync.
async fn sync_session_to_peers(
    session_registry: &Arc<Mutex<HashMap<String, SharedEntry>>>,
    session_id: &str,
    exclude: &Tx,
) {
    let reg = session_registry.lock().await;
    if let Some(entry) = reg.get(session_id) {
        let updated: Vec<serde_json::Value> = entry
            .handle
            .lock()
            .await
            .as_ref()
            .map(|h| {
                h.messages
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect()
            })
            .unwrap_or_default();
        let ml = ServerMsg::MessagesLoaded { messages: updated };
        let mut dead = Vec::new();
        for (i, peer) in entry.txs.iter().enumerate() {
            if Arc::ptr_eq(peer, exclude) {
                continue;
            }
            if !send_msg_ok(peer, &ml).await {
                dead.push(i);
            }
        }
        // Can't remove while iterating immutably — drop reg, re-lock.
        drop(reg);
        if !dead.is_empty() {
            let mut reg = session_registry.lock().await;
            if let Some(entry) = reg.get_mut(session_id) {
                for i in dead.into_iter().rev() {
                    entry.txs.remove(i);
                }
            }
        }
    }
}

/// Like `send_msg` but returns `true` on success so the caller can detect
/// dead connections (e.g. during broadcasts) without logging a warning.
async fn send_msg_ok(tx: &Tx, msg: &ServerMsg) -> bool {
    if let Ok(text) = serde_json::to_string(msg) {
        tx.lock()
            .await
            .send(WsMessage::Text(text))
            .await
            .is_ok()
    } else {
        false
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
    append: Option<String>,
    arguments: Option<String>,
    compact_model: Option<String>,
    tx: Tx,
    pending_permissions: Arc<PermissionMap>,
    permission_mode: nonoclaw_core::PermissionMode,
    skills_manager: Arc<RwLock<SkillsManager>>,
    background_registry: Arc<std::sync::Mutex<nonoclaw_tools::BackgroundTaskRegistry>>,
) -> EngineOptions {
    const DEFAULT_MAX_TURNS: u32 = 200;
    const DEFAULT_MODEL: &str = "claude-sonnet-4-5-20250929";
    const DEFAULT_MAX_TOKENS: u32 = 8192;
    const DEFAULT_COMPACT_THRESHOLD: usize = 80_000;
    let permission_resolver = make_permission_resolver(tx, pending_permissions);

    EngineOptions {
        model: model.unwrap_or_else(|| DEFAULT_MODEL.into()),
        max_tokens: DEFAULT_MAX_TOKENS,
        permission_mode,
        allowed_tools: vec![],
        disallowed_tools: vec![],
        add_dirs: vec![],
        max_turns: max_turns.unwrap_or(DEFAULT_MAX_TURNS),
        append_system_prompt: append.clone(),
        arguments,
        thinking: None,
        is_non_interactive: false,
        permission_resolver: Some(permission_resolver),
        question_resolver: None, // set per-engine below
        auto_compact: true,
        compact_threshold_tokens: 80_000,
        compact_model,
        chars_per_token: 4,
        context_window: None, // resolved at run time via apply_model_profile
        skills_manager: Some(skills_manager),
        background_registry: Some(background_registry),
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
    if let Some(home) = nonoclaw_core::home_dir() {
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
const FILE_TREE_MAX_DEPTH: u32 = 10;
const FILE_TREE_MAX_ENTRIES: usize = 10000;

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
        // Skip hidden entries and generated/heavy dirs.
        if name.starts_with('.') { continue; }
        if is_dir && FILE_TREE_SKIP_DIRS.iter().any(|s| *s == name) { continue; }
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

/// Spawn `cloudflared tunnel --url <local_addr>` and read its stderr for the
/// public `*.trycloudflare.com` URL. Leaves the process running in background;
/// it is killed automatically when the server exits (kill_on_drop).
async fn spawn_tunnel(local_addr: &str) -> Option<String> {
    let port = local_addr.rsplit(':').next()?;
    let target = format!("http://127.0.0.1:{port}");
    tracing::info!(%target, %local_addr, "spawning cloudflared tunnel");

    let mut child = match tokio::process::Command::new("cloudflared")
        .args(["tunnel", "--no-autoupdate", "--url", &target])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("cloudflared not found in PATH ({e}) — install it: curl -L https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 -o ~/bin/cloudflared && chmod +x ~/bin/cloudflared");
            return None;
        }
    };

    let stderr = child.stderr.take()?;
    let mut reader = tokio::io::BufReader::new(stderr);
    use tokio::io::AsyncBufReadExt;
    let mut buf = String::new();

    let found: Option<String> = loop {
        buf.clear();
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(12));
        tokio::pin!(timeout);
        tokio::select! {
            r = reader.read_line(&mut buf) => match r {
                Ok(0) => break None,
                Ok(_) => {
                    if buf.contains("trycloudflare.com") {
                        if let Some(pos) = buf.find("https://") {
                            let rest = &buf[pos..];
                            let url = rest.split_whitespace()
                                .next()
                                .unwrap_or(rest)
                                .trim_end_matches(|c: char| c == '.' || c == '|' || c == ' ');
                            break Some(url.to_string());
                        }
                    }
                }
                Err(_) => break None,
            },
            _ = &mut timeout => {
                tracing::warn!("timed out waiting for cloudflared URL");
                break None;
            }
        }
    };

    // Move reader + child into background so the stderr pipe stays open
    // (no EPIPE → no SIGPIPE → cloudflared keeps running).
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let _ = tokio::io::copy(&mut reader.into_inner(), &mut tokio::io::sink()).await;
        let _ = child.wait().await;
    });
    if found.is_some() {
        tracing::info!("tunnel URL captured — waiting for edge activation...");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }

    found
}

// ── Upload handler ──────────────────────────────────────────────────────────

async fn upload_handler(
    State(state): State<Arc<AppState>>,
    mut multipart: axum::extract::Multipart,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::StatusCode;

    // Check doc model is configured.
    let doc_model = match &state.doc_model {
        Some(c) if c.is_enabled() => c.clone(),
        _ => {
            return axum::response::Response::builder()
                .status(StatusCode::NOT_IMPLEMENTED)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"error":"document processing not configured; set docModel in settings.json"}"#,
                ))
                .unwrap();
        }
    };

    // Parse the first file field.
    let mut file_bytes: Vec<u8> = Vec::new();
    let mut filename = String::new();

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            filename = field
                .file_name()
                .unwrap_or("untitled")
                .to_string();
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        file_bytes.extend_from_slice(chunk.as_ref());
                        if file_bytes.len() > attachments::MAX_FILE_SIZE as usize {
                            return axum::response::Response::builder()
                                .status(StatusCode::PAYLOAD_TOO_LARGE)
                                .header("content-type", "application/json")
                                .body(Body::from(format!(
                                    r#"{{"error":"file exceeds {} MB limit"}}"#,
                                    attachments::MAX_FILE_SIZE / (1024 * 1024)
                                )))
                                .unwrap();
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("multipart chunk error: {e}");
                        break;
                    }
                }
            }
        }
    }

    if file_bytes.is_empty() {
        return axum::response::Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"error":"no file provided"}"#))
            .unwrap();
    }

    // Validate extension.
    if !attachments::is_allowed_extension(&filename) {
        return axum::response::Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"error":"unsupported file type; allowed: {}"}}"#,
                attachments::ALLOWED_EXTENSIONS.join(", ")
            )))
            .unwrap();
    }

    // Write file to disk.
    let upload_id = Uuid::new_v4().to_string();
    let safe_name = attachments::sanitize_filename(&filename);
    let file_dir = state.upload_dir.join(&upload_id);
    if let Err(e) = std::fs::create_dir_all(&file_dir) {
        return axum::response::Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"error":"storage error: {e}"}}"#)))
            .unwrap();
    }
    let stored_path = file_dir.join(&safe_name);
    if let Err(e) = std::fs::write(&stored_path, &file_bytes) {
        return axum::response::Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"error":"write error: {e}"}}"#)))
            .unwrap();
    }

    // Process the file through the doc model.
    let extracted = attachments::process_file(&doc_model, &stored_path, &safe_name, &upload_id).await;

    let images = if extracted.images_base64.is_empty() {
        None
    } else {
        Some(extracted.images_base64.iter().map(|i| ImageRef {
            media_type: i.media_type.clone(),
            data: i.data.clone(),
        }).collect())
    };

    let resp = UploadResponse {
        id: extracted.id,
        filename: extracted.filename,
        extracted_text: extracted.extracted_text,
        image_count: extracted.image_count,
        images,
        error: extracted.error,
    };

    let body = match serde_json::to_string(&resp) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"serialization error: {e}"}}"#),
    };

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
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
    public_url: Option<String>,
    tunnel: bool,
    model_profiles: Vec<ModelProfile>,
    doc_model: Option<nonoclaw_engine::settings::DocModelConfig>,
    compact_model: Option<String>,
) -> anyhow::Result<()> {
    // Bind the listener FIRST so the port is open before cloudflared tries
    // to connect (otherwise tunnel spawn races the bind and gets "connection
    // refused" from the OS).
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("NonoClaw web UI listening on http://{addr}");

    // Spawn the tunnel after the port is confirmed open.
    let public_url = if tunnel {
        let tunnel_url = spawn_tunnel(addr).await;
        if tunnel_url.is_some() {
            tracing::info!(url = ?tunnel_url, "cloudflared tunnel ready");
            tunnel_url
        } else {
            public_url
        }
    } else {
        public_url
    };

    let auth_token = Uuid::new_v4().to_string().replace('-', "");
    let active_model = model_profiles.iter()
        .find(|p| p.default)
        .or_else(|| model_profiles.first())
        .map(|p| p.name.clone())
        .unwrap_or_else(|| model.clone());
    tracing::info!(auth_token, %active_model, "mobile auth token generated");

    // File upload storage: ~/.nonoclaw/projects/<cwd>/uploads/
    let upload_dir = nonoclaw_engine::session::home_root()
        .map(|r| r.join("projects").join(
            cwd.to_string_lossy()
                .trim_start_matches('/')
                .replace('/', "-")
        ).join("uploads"))
        .unwrap_or_else(|| cwd.join(".nonoclaw/uploads"));
    if let Err(e) = std::fs::create_dir_all(&upload_dir) {
        tracing::warn!(dir=%upload_dir.display(), "cannot create upload dir: {e}");
    }

    let state = Arc::new(AppState {
        model_profiles,
        active_model: Arc::new(Mutex::new(active_model)),
        client,
        registry,
        todos,
        cwd: cwd.clone(),
        model,
        mcp_configs,
        mcp_sources,
        context_window,
        compact_threshold,
        auth_token,
        public_url,
        session_registry: Arc::new(Mutex::new(HashMap::new())),
        pending_permissions: Arc::new(Mutex::new(HashMap::new())),
        pending_questions: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: Arc::new(Mutex::new(nonoclaw_core::PermissionMode::Default)),
        skills_manager: Arc::new(RwLock::new(SkillsManager::new(&cwd))),
        background_registry: Arc::new(std::sync::Mutex::new(
            nonoclaw_tools::BackgroundTaskRegistry::new(),
        )),
        doc_model,
        compact_model,
        upload_dir,
    });

    // Spawn file watcher for hot-reloading skills.
    crate::skill_watcher::spawn_skill_watcher(
        Arc::clone(&state.skills_manager),
        cwd.clone(),
    );

    // Print QR code if a public URL is available (from --tunnel or --public-url).
    if let Some(ref url) = state.public_url {
        let full = format!("{url}/?token={}", state.auth_token);
        use qrcode::QrCode;
        match QrCode::new(&full) {
            Ok(qr) => {
                let s = qr.render::<char>().quiet_zone(true).module_dimensions(4, 2).build();
                let bar = "═".repeat(56);
                eprintln!("\n\x1b[1;36m{bar}");
                eprintln!("{s}"); eprintln!("{bar}\x1b[0m");
            }
            Err(_) => {
                let bar = "═".repeat(full.len() + 4);
                eprintln!("\n\x1b[1;36m ╔{bar}╗\n ║  {full}  ║\n ╚{bar}╝\x1b[0m");
            }
        }
        eprintln!("  \x1b[1;33m{full}\x1b[0m\n  \x1b[1;33mScan with your phone\x1b[0m\n");
    }

    // Always register the WebSocket route + PWA manifest + service worker.
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/upload", axum::routing::post(upload_handler))
        .route("/manifest.json", get(serve_manifest))
        .route("/sw.js", get(serve_sw))
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

    axum::serve(listener, app).await?;
    Ok(())
}

// ── WebSocket handler ───────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // Validate auth token if present in query. No token = backward-compat.
    if let Some(token) = params.get("token") {
        if token != &state.auth_token {
            return axum::response::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from("invalid or missing auth token"))
                .unwrap();
        }
    }
    let session_id = params.get("session").cloned();
    ws.on_upgrade(move |socket| handle_ws(socket, state, session_id))
}

/// ── PWA manifest.json ────────────────────────────────────────────────────
async fn serve_manifest() -> impl IntoResponse {
    let icon = "data:image/svg+xml;charset=utf-8,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 192 192'%3E%3Crect width='192' height='192' rx='38' fill='%23070a0f'/%3E%3Ctext x='96' y='124' font-family='serif' font-size='88' font-style='italic' fill='%235eead4' text-anchor='middle'%3ENC%3C/text%3E%3C/svg%3E";
    let body = serde_json::json!({
        "name": "NonoClaw",
        "short_name": "NonoClaw",
        "start_url": "/",
        "display": "standalone",
        "background_color": "#070a0f",
        "theme_color": "#5eead4",
        "icons": [
            { "src": icon, "sizes": "192x192", "type": "image/svg+xml" },
            { "src": icon, "sizes": "512x512", "type": "image/svg+xml" }
        ]
    })
    .to_string();
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/manifest+json")
        .body(Body::from(body))
        .unwrap()
}

/// ── PWA service worker ───────────────────────────────────────────────────
async fn serve_sw() -> impl IntoResponse {
    let body = r#"const C="nc-v3";self.addEventListener("install",e=>{e.waitUntil(self.skipWaiting())});self.addEventListener("activate",e=>{e.waitUntil((async()=>{await self.clients.claim();const keys=await caches.keys();for(const k of keys){if(k!==C)await caches.delete(k)}})())});self.addEventListener("fetch",e=>{const u=new URL(e.request.url);if(u.pathname.startsWith("/assets/")){e.respondWith(caches.open(C).then(c=>c.match(e.request).then(r=>r||fetch(e.request).then(res=>{c.put(e.request,res.clone());return res}))))}else if(u.pathname==="/ws"){return}else{e.respondWith(fetch(e.request))}});"#;
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/javascript")
        .body(Body::from(body))
        .unwrap()
}

async fn handle_ws(ws: WebSocket, state: Arc<AppState>, session_id: Option<String>) {
    let (tx, mut rx) = ws.split();
    let tx: Tx = Arc::new(Mutex::new(tx));
    let mut cancel: Option<CancellationToken> = None;
    /// JoinHandle of the current run — aborted on Cancel to stop the engine hard.
    let mut run_handle: Option<tokio::task::JoinHandle<()>> = None;
    /// Consumer abort handle, shared between the spawned run task and the
    /// main handler loop (Clear / Cancel).  Wrapped in Arc<Mutex> because
    /// it is set inside the spawned task and read from the outer scope.
    let consumer_handle: Arc<tokio::sync::Mutex<Option<tokio::task::AbortHandle>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // Capture before any inner shadow (the Run arm destructures session_id).
    // Desktop (no URL param): auto-resume the most recent session.
    // Mobile (QR code): use the session id encoded in the QR.
    let shared_sid = session_id
        .clone()
        .or_else(|| nonoclaw_engine::most_recent_session(&state.cwd).ok().flatten());

    // Register this Tx in the shared session if we have a session id
    // (desktop auto-continue or mobile QR); otherwise use per-connection.
    if let Some(ref sid) = shared_sid {
        let mut reg = state.session_registry.lock().await;
        if let Some(entry) = reg.get_mut(sid) {
            entry.txs.push(tx.clone());
        } else {
            // First registration: try to load the session from disk. Only
            // create a fresh one if the file is genuinely missing. If resume
            // fails for other reasons, log it but still create a fresh one
            // so the user can work (the old data is gone anyway).
            let h = match resume_session(&state, sid) {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!("session `{sid}` resume failed: {e} — creating fresh");
                    create_new_session(&state)
                }
            };
            reg.insert(
                sid.clone(),
                SharedEntry {
                    handle: Arc::new(Mutex::new(h)),
                    txs: vec![tx.clone()],
                },
            );
        }
    }

    // Per-connection session: either the shared handle (if session_id is set)
    // or a fresh per-connection one.
    let session: SharedHandle = if let Some(ref sid) = shared_sid {
        let reg = state.session_registry.lock().await;
        reg.get(sid)
            .map(|e| Arc::clone(&e.handle))
            .unwrap_or_else(|| Arc::new(Mutex::new(None)))
    } else {
        Arc::new(Mutex::new(None))
    };

    // Connect handshake.
    {
        send_msg(
            &tx,
            ServerMsg::SessionList {
                sessions: list_sessions_wire(&state),
            },
        )
        .await;

        // If sharing an existing session, replay its messages so the connecting
        // peer (e.g. mobile) sees the same conversation. Otherwise, fresh.
        let existing = session.lock().await.clone();
        let h = existing.unwrap_or_else(|| {
            create_new_session(&state).unwrap_or_else(|| SessionHandle {
                id: "no-store".into(),
                file: PathBuf::new(),
                messages: Vec::new(),
            })
        });

        let vals: Vec<serde_json::Value> =
            h.messages.iter().filter_map(|m| serde_json::to_value(m).ok()).collect();
        let sid = h.id.clone();
        *session.lock().await = Some(h);

        send_msg(&tx, ServerMsg::MessagesLoaded { messages: vals }).await;
        send_msg(
            &tx,
            ServerMsg::Info {
                model: state.active_model.lock().await.clone(),
                auth_token: state.auth_token.clone(),
                available_models: state.model_profiles.iter().filter(|p| p.is_conversation_model()).map(|p| ModelInfo {
                    name: p.name.clone(),
                    label: p.label.clone().unwrap_or_else(|| p.name.clone()),
                    context_window: p.context_window,
                }).collect(),
                session_id: sid,
            },
        )
        .await;

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
        let skills_snapshot = state.skills_manager.read().unwrap().all_active();
        let current_model = state.active_model.lock().await.clone();
        let info = gather(
            &state.cwd,
            &current_model,
            &state.registry,
            &state.mcp_configs,
            &state.mcp_sources,
            state.context_window,
            state.compact_threshold,
            state.public_url.clone(),
            &skills_snapshot,
        )
        .await;
        send_msg(&tx, ServerMsg::ProjectInfo { info }).await;
    }

    // Server-side keepalive: send a lightweight data frame every 8s. Browser
    // WS Ping/Pong frames do NOT fire onmessage, so the client can't track
    // liveness from them — a data heartbeat lets the client detect a frozen
    // (half-dead) socket via its lastMsgAt timer and reconnect on send.
    let tx_ping = Arc::clone(&tx);
    let ping_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(8));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            let mut guard = tx_ping.lock().await;
            if guard
                .send(WsMessage::Text(r#"{"type":"ping"}"#.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

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
                                model: state.active_model.lock().await.clone(),
                                auth_token: state.auth_token.clone(),
                available_models: state.model_profiles.iter().filter(|p| p.is_conversation_model()).map(|p| ModelInfo {
                    name: p.name.clone(),
                    label: p.label.clone().unwrap_or_else(|| p.name.clone()),
                    context_window: p.context_window,
                }).collect(),
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
                            model: state.active_model.lock().await.clone(),
                            auth_token: state.auth_token.clone(),
                available_models: state.model_profiles.iter().filter(|p| p.is_conversation_model()).map(|p| ModelInfo {
                    name: p.name.clone(),
                    label: p.label.clone().unwrap_or_else(|| p.name.clone()),
                    context_window: p.context_window,
                }).collect(),
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
                // Re-scan for new skill directories the file watcher may have missed.
                state.skills_manager.write().unwrap().rescan(&state.cwd);
                let skills_snapshot = state.skills_manager.read().unwrap().all_active();
                let current_model = state.active_model.lock().await.clone();
                let profile = state.model_profiles.iter().find(|p| p.name == current_model);
                let cw = profile.and_then(|p| p.context_window).or(state.context_window);
                let ct = profile.and_then(|p| {
                    p.context_window.map(|cw| cw.saturating_sub(p.max_tokens.unwrap_or(8192) as usize + 2048))
                }).unwrap_or(state.compact_threshold);
                // Re-scan MCP configs for newly added servers.
                let live_mcp = refresh_mcp_configs(&state.cwd, &state.mcp_configs);
                let mcp_sources_clone = state.mcp_sources.clone();
                let info = gather(
                    &state.cwd,
                    &current_model,
                    &state.registry,
                    &live_mcp,
                    &mcp_sources_clone,
                    cw,
                    ct,
                    state.public_url.clone(),
                    &skills_snapshot,
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
                append_system_prompt,
                arguments,
                attachments,
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

                // Immediately sync the session state to other peers
                // so mobile / other tabs see the incoming user message.
                if let Some(ref cid) = shared_sid {
                    sync_session_to_peers(&state.session_registry, cid, &tx).await;
                }

                let tx2 = tx.clone();
                let s = state.clone();
                let session2 = Arc::clone(&session);
                let sync_sid = shared_sid.clone();
                let model_used = if let Some(m) = model.clone() {
                    m
                } else {
                    s.active_model.lock().await.clone()
                };

                // Fork context: if the user typed /skill-name and the skill
                // has context: "fork", execute it as an isolated sub-agent
                // instead of injecting inline.
                let fork_body: Option<String> = {
                    let mgr = state.skills_manager.read().unwrap();
                    // Extract skill name from prompt: "/name args..." -> "name"
                    let skill_name = prompt
                        .strip_prefix('/')
                        .and_then(|rest| rest.split_whitespace().next())
                        .unwrap_or("");
                    if !skill_name.is_empty() {
                        if let Some(skill) = mgr.get_skill(skill_name) {
                            if skill.context.as_deref() == Some("fork") {
                                let args = arguments.as_deref().unwrap_or("");
                                let sid = &session_id;
                                let body = substitute_arguments(
                                    &skill.body,
                                    args,
                                    &skill.argument_names,
                                    Some(&skill.source),
                                    Some(sid),
                                );
                                tracing::info!(
                                    name = skill_name,
                                    "executing skill in fork context"
                                );
                                Some(body)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                let ch = Arc::clone(&consumer_handle);
                run_handle = Some(tokio::spawn(async move {
                    let _ch = ch;
                    let request_id = Uuid::new_v4().to_string();

                    // If executing in fork context: run as a fresh sub-engine.
                    if let Some(body) = fork_body {
                        let fork_request_id = Uuid::new_v4().to_string();
                        let qr: Arc<dyn QuestionResolver> = Arc::new(WsQuestionResolver {
                            request_id: format!("{fork_request_id}-q"),
                            pending: Arc::clone(&s.pending_questions),
                            tx: tx2.clone(),
                        });
                        let fork_opts = {
                            let mut o = build_options(
                                Some(model_used.clone()),
                                None,
                                Some(body.clone()),
                                arguments.clone(),
                                s.compact_model.clone(),
                                tx2.clone(),
                                Arc::clone(&s.pending_permissions),
                                *s.permission_mode.lock().await,
                                Arc::clone(&s.skills_manager),
                                Arc::clone(&s.background_registry),
                            );
                            o.max_turns = o.max_turns.min(20);
                            o.is_non_interactive = true;
                            o.question_resolver = Some(qr);
                            o
                        };
                        let mut fork_engine = QueryEngine::with_session(
                            s.client.clone(),
                            s.registry.clone(),
                            s.todos.clone(),
                            fork_opts,
                            Vec::new(),
                            format!("fork-{}", Uuid::new_v4()),
                            None,
                        );
                        match fork_engine.run(MessageContent::from_text(&body), &s.cwd, |_ev| {}).await {
                            Ok(result) => {
                                send_msg(&tx2, ServerMsg::Done {
                                    text: result.text,
                                    usage: serde_json::to_value(result.usage).unwrap_or_default(),
                                    turns: result.turns,
                                    stop_reason: result.stop_reason.as_ref().map(|s| s.as_str().to_string()),
                                }).await;
                            }
                            Err(e) => {
                                send_msg(&tx2, ServerMsg::Error {
                                    message: format!("fork execution failed: {e}"),
                                }).await;
                            }
                        }
                        return;
                    }

                    let mut options = build_options(
                        Some(model_used.clone()),
                        max_turns,
                        append_system_prompt.clone(),
                        arguments.clone(),
                        s.compact_model.clone(),
                        tx2.clone(),
                        Arc::clone(&s.pending_permissions),
                        *s.permission_mode.lock().await,
                        Arc::clone(&s.skills_manager),
                        Arc::clone(&s.background_registry),
                    );

                    // Apply per-model overrides (contextWindow, maxTokens, charsPerToken).
                    if let Some(profile) = s.model_profiles.iter().find(|p| p.name == model_used) {
                        options.apply_model_profile(profile);
                    }

                    // Question resolver (per-run to avoid oneshot key clashes).
                    let qr: Arc<dyn QuestionResolver> = Arc::new(WsQuestionResolver {
                        request_id: format!("{request_id}-q"),
                        pending: Arc::clone(&s.pending_questions),
                        tx: tx2.clone(),
                    });
                    options.question_resolver = Some(qr);

                    // Multi-model: if the requested model has its own
                    // base_url/api_key in the profiles, build a dedicated
                    // Client for this run. Otherwise reuse the default.
                    let run_client: Arc<nonoclaw_api::Client> = {
                        let profile = s.model_profiles.iter()
                            .find(|p| p.name == model_used);
                        match profile {
                            Some(p) if p.base_url != s.client.base_url() || p.api_key != s.client.api_key().unwrap_or_default() => {
                                match nonoclaw_api::Client::new(
                                    Some(p.api_key.clone()),
                                    None,
                                    p.base_url.clone(),
                                ) {
                                    Ok(c) => {
                                        tracing::info!(model=%model_used, url=%p.base_url, "per-run client rebuilt");
                                        Arc::new(c)
                                    }
                                    Err(e) => {
                                        tracing::warn!("per-run client build failed ({e}) — falling back to default");
                                        s.client.clone()
                                    }
                                }
                            }
                            _ => s.client.clone(),
                        }
                    };

                    let mut engine = QueryEngine::with_session(
                        run_client,
                        s.registry.clone(),
                        s.todos.clone(),
                        options,
                        prior,
                        session_id,
                        Some(session_file),
                    );

                    // Enrich the prompt with attachment content + images.
                    let enriched = enrich_prompt_with_attachments(&prompt, &attachments);

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
                                break;
                            }
                        }
                    });
                    // Stash handle so Clear/Cancel can abort consumer before
                    // sending MessagesLoaded — prevents tool-card residue.
                    *_ch.lock().await = Some(consumer.abort_handle());

                    tracing::debug!("starting engine run (attachments: {})", attachments.as_ref().map(|a| a.len()).unwrap_or(0));
                    let result = engine
                        .run(enriched, &s.cwd, |ev| {
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
                            let skills_snapshot = s.skills_manager.read().unwrap().all_active();
                            let current_model = s.active_model.lock().await.clone();
                            let info = gather(
                                &s.cwd,
                                &current_model,
                                &s.registry,
                                &s.mcp_configs,
                                &s.mcp_sources,
                                s.context_window,
                                s.compact_threshold,
                                s.public_url.clone(),
                                &skills_snapshot,
                            )
                            .await;
                            send_msg(&tx2, ServerMsg::ProjectInfo { info: info.clone() }).await;

                            // Broadcast updated messages + project info to all
                            // other peers sharing this session.
                            if let Some(ref cid) = sync_sid {
                                sync_session_to_peers(&s.session_registry, cid, &tx2).await;
                                // Also push ProjectInfo refresh.
                                let pi = ServerMsg::ProjectInfo { info };
                                let reg = s.session_registry.lock().await;
                                if let Some(entry) = reg.get(cid) {
                                    let mut dead = Vec::new();
                                    for (i, peer) in entry.txs.iter().enumerate() {
                                        if Arc::ptr_eq(peer, &tx2) { continue; }
                                        if !send_msg_ok(peer, &pi).await { dead.push(i); }
                                    }
                                    drop(reg);
                                    if !dead.is_empty() {
                                        let mut reg = s.session_registry.lock().await;
                                        if let Some(entry) = reg.get_mut(cid) {
                                            for i in dead.into_iter().rev() { entry.txs.remove(i); }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "engine run failed");
                            if !new_cancel.is_cancelled() {
                                send_msg(&tx2, ServerMsg::Error { message: e.to_string() }).await;
                            }
                        }
                    }
                }));
            }

            // ── Cancel ──────────────────────────────────────────────────────
            ClientMsg::Cancel => {
                if let Some(ref c) = cancel {
                    c.cancel();
                }
                // Abort the spawned task to stop the engine immediately —
                // CancellationToken only gates between turns, not during
                // an in-flight API streaming call.
                // Abort the spawned task to stop engine + consumer immediately.
                if let Some(h) = run_handle.take() {
                    h.abort();
                }
                if let Some(ch) = consumer_handle.lock().await.take() {
                    ch.abort();
                }
                cancel = None;
                {
                    // Notify the UI that the run was cancelled so agentRunning
                    // flips back to false.
                    send_msg(&tx, ServerMsg::Done {
                        text: "Run cancelled.".into(),
                        usage: serde_json::json!({}),
                        turns: 0,
                        stop_reason: Some("cancelled".into()),
                    }).await;
                }
            }

            // ── Switch permission mode at runtime ──────────────────────────
            ClientMsg::SetPermissionMode { mode } => {
                let new_mode = match mode.as_str() {
                    "auto" => nonoclaw_core::PermissionMode::Auto,
                    "bypass" | "bypassPermissions" => nonoclaw_core::PermissionMode::BypassPermissions,
                    "plan" => nonoclaw_core::PermissionMode::Plan,
                    "acceptEdits" => nonoclaw_core::PermissionMode::AcceptEdits,
                    _ => nonoclaw_core::PermissionMode::Default,
                };
                *state.permission_mode.lock().await = new_mode;
                tracing::info!(?new_mode, "permission mode switched");
            }

            // ── Switch active model ────────────────────────────────────
            ClientMsg::SetModel { name } => {
                // Verify the model exists in the profiles.
                if state.model_profiles.iter().any(|p| p.name == name) {
                    // Also apply env vars for the new model (needed for
                    // non-multi-model runs that still use from_env).
                    for p in &state.model_profiles {
                        if p.name == name {
                            if std::env::var_os("ANTHROPIC_BASE_URL").is_none() ||
                               std::env::var("ANTHROPIC_BASE_URL").unwrap_or_default() != p.base_url {
                                std::env::set_var("ANTHROPIC_BASE_URL", &p.base_url);
                            }
                            if std::env::var_os("ANTHROPIC_API_KEY").is_none() ||
                               std::env::var("ANTHROPIC_API_KEY").unwrap_or_default() != p.api_key {
                                std::env::set_var("ANTHROPIC_API_KEY", &p.api_key);
                            }
                        }
                    }
                    *state.active_model.lock().await = name.clone();
                    tracing::info!(%name, "active model switched");
                    // Resolve per-model context_window + compact_threshold.
                    let profile = state.model_profiles.iter().find(|p| p.name == name);
                    let cw = profile.and_then(|p| p.context_window)
                        .or(state.context_window);
                    let ct = profile.and_then(|p| {
                        p.context_window.map(|cw| cw.saturating_sub(p.max_tokens.unwrap_or(8192) as usize + 2048))
                    }).unwrap_or(state.compact_threshold);
                    // Push updated Info + ProjectInfo so the UI reflects the new model immediately.
                    send_msg(&tx, ServerMsg::Info {
                        model: name.clone(),
                        auth_token: state.auth_token.clone(),
                        available_models: state.model_profiles.iter().filter(|p| p.is_conversation_model()).map(|p| ModelInfo {
                            name: p.name.clone(),
                            label: p.label.clone().unwrap_or_else(|| p.name.clone()),
                            context_window: p.context_window,
                        }).collect(),
                        session_id: session.lock().await.as_ref().map(|h| h.id.clone()).unwrap_or_default(),
                    }).await;
                    let skills_snapshot = state.skills_manager.read().unwrap().all_active();
                    let info = gather(
                        &state.cwd,
                        &name,
                        &state.registry,
                        &state.mcp_configs,
                        &state.mcp_sources,
                        cw,
                        ct,
                        state.public_url.clone(),
                        &skills_snapshot,
                    ).await;
                    send_msg(&tx, ServerMsg::ProjectInfo { info }).await;
                } else {
                    tracing::warn!("unknown model `{name}` — ignored");
                }
            }

            // ── Clear (in-memory only; on-disk transcript is the archive) ───
            ClientMsg::Clear => {
                // Cancel + abort engine + consumer so no buffered tool events
                // can arrive after we send MessagesLoaded.
                if let Some(ref c) = cancel {
                    c.cancel();
                }
                if let Some(h) = run_handle.take() {
                    h.abort();
                }
                if let Some(ch) = consumer_handle.lock().await.take() {
                    ch.abort();
                }
                cancel = None;
                // Brief yield so the consumer task has time to observe the
                // abort and stop processing its channel (prevents events
                // arriving after MessagesLoaded).
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                // Now clear the transcript.
                if let Some(h) = session.lock().await.as_mut() {
                    h.messages.clear();
                    // Truncate the on-disk JSONL back to header-only.
                    if let Err(e) = nonoclaw_engine::clear_session(&h.file) {
                        tracing::warn!("failed to clear session file: {e}");
                    }
                }
                let ml = ServerMsg::MessagesLoaded { messages: vec![] };
                send_msg(&tx, ml.clone()).await;

                // Broadcast the clear to all other peers.
                if let Some(ref cid) = shared_sid {
                    sync_session_to_peers(&state.session_registry, cid, &tx).await;
                }
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
                    None,
                    None,
                    state.compact_model.clone(),
                    tx.clone(),
                    Arc::clone(&state.pending_permissions),
                    *state.permission_mode.lock().await,
                    Arc::clone(&state.skills_manager),
                    Arc::clone(&state.background_registry),
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
                        let count = handle_owned.messages.len();
                        handle_owned.messages = eng.take_messages();
                        *session.lock().await = Some(handle_owned);
                        // Nothing to compact — tell the UI.
                        let ev = serde_json::json!({
                            "kind": "compacted",
                            "removed": 0,
                            "kept": count,
                            "tokens_before": 0,
                            "tokens_after": 0,
                        });
                        send_msg(&tx, ServerMsg::Event { event: ev }).await;
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
    // Loop exited — stop the keepalive pinger for this connection.
    ping_handle.abort();
    // Connection closed — remove this Tx from the shared session's broadcast
    // list so we don't keep trying to send to a dead peer.
    if let Some(ref sid) = shared_sid {
        let mut reg = state.session_registry.lock().await;
        let remove = if let Some(entry) = reg.get_mut(sid) {
            entry.txs.retain(|t| !Arc::ptr_eq(t, &tx));
            entry.txs.is_empty()
        } else {
            false
        };
        if remove {
            reg.remove(sid);
        }
    }
}
