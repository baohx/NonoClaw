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
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::http::{HeaderMap, StatusCode};
use axum::{
    extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    extract::{ConnectInfo, Query, State},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use nonoclaw_core::{AppError, ErrorCode, MessageContent, PermissionDecision};
use nonoclaw_engine::{
    substitute_arguments, ClientPurpose, QueryEngine, ResolvedConfig, RunContext, RunController,
    RunEvent, RunLimits, RunTerminalStatus, SessionService, SkillsManager,
};
use nonoclaw_tools::tool::QuestionResolver;
use nonoclaw_tools::{TodoStore, ToolRegistry};
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use uuid::Uuid;

// ── Shared protocol and resolver aliases ───────────────────────────────────

#[cfg(test)]
use super::protocol::WS_PROTOCOL_VERSION;
use super::protocol::{
    event_message, messages_loaded, safe_error, send_msg, send_msg_ok, synthetic_event_message,
    terminal_fields, ClientMsg, ModelInfo, ServerMsg, SessionInfoWire,
};
#[cfg(test)]
use crate::project_info::ProjectInfo;
#[cfg(test)]
use nonoclaw_engine::{EngineEvent, EventEnvelope};

use super::project_service::ProjectService;
use super::run_handler::{
    build_options, enrich_prompt_with_attachments, PermissionMap, QuestionMap, WsQuestionResolver,
};
use super::session_hub::{create_new_session, resume_session, SessionHub, SharedHandle};

// ── Shared application state ────────────────────────────────────────────────

pub(super) struct AppState {
    registry: Arc<ToolRegistry>,
    todos: Arc<TodoStore>,
    cwd: PathBuf,
    /// Canonical immutable configuration snapshot shared by all Web paths.
    pub(super) config: Arc<ResolvedConfig>,
    /// Auth token for remote (QR-code) mobile access.
    auth_token: String,
    /// Public/tunnel listeners require the token; loopback remains low-friction.
    require_auth: bool,
    /// The public URL shown in the QR code, or None.
    public_url: Option<String>,
    /// Currently active model name (switchable via SetModel client message + UI).
    active_model: Arc<Mutex<String>>,
    /// Canonical owner for session discovery and per-session writer actors.
    session_service: SessionService,
    /// Session peer registration and revisioned broadcast owner.
    session_hub: SessionHub,
    pending_permissions: Arc<PermissionMap>,
    pending_questions: Arc<QuestionMap>,
    /// Runtime-mutable permission mode (switchable via UI).
    permission_mode: Arc<Mutex<nonoclaw_core::PermissionMode>>,
    /// Skill manager: discovers, parses, and dynamically activates skills.
    skills_manager: Arc<RwLock<SkillsManager>>,
    /// Deduplicated owner of git/config/skills ProjectInfo and file operations.
    project_service: Arc<ProjectService>,
    /// Background task registry for run_in_background bash commands.
    background_registry: Arc<std::sync::Mutex<nonoclaw_tools::BackgroundTaskRegistry>>,
    /// Directory where uploaded attachments are stored.
    pub(super) upload_dir: PathBuf,
}

impl AppState {
    pub(super) fn authorized(&self, supplied_token: Option<&str>) -> bool {
        token_is_authorized(self.require_auth, &self.auth_token, supplied_token)
    }

    fn websocket_authorized(
        &self,
        supplied_token: Option<&str>,
        peer_ip: IpAddr,
        headers: &HeaderMap,
    ) -> bool {
        let forwarded = headers.contains_key("forwarded")
            || headers.contains_key("x-forwarded-for")
            || headers.contains_key("x-real-ip")
            || headers.contains_key("cf-connecting-ip");
        let require_auth = request_requires_auth(self.require_auth, peer_ip, forwarded);
        token_is_authorized(require_auth, &self.auth_token, supplied_token)
    }
}

fn request_requires_auth(configured: bool, peer_ip: IpAddr, forwarded: bool) -> bool {
    configured && (!peer_ip.is_loopback() || forwarded)
}

#[cfg(test)]
pub(super) fn upload_exploration_state(
    cwd: PathBuf,
    config: Arc<ResolvedConfig>,
    upload_dir: PathBuf,
) -> Arc<AppState> {
    let (registry, todos) = nonoclaw_tools::register_all();
    let registry = Arc::new(registry);
    let skills_manager = Arc::new(RwLock::new(SkillsManager::new(&cwd)));
    let project_service = Arc::new(ProjectService::new(
        cwd.clone(),
        Arc::clone(&registry),
        Arc::clone(&config),
        None,
        Arc::clone(&skills_manager),
    ));
    Arc::new(AppState {
        registry,
        todos,
        cwd,
        config: Arc::clone(&config),
        auth_token: "exploration-token".into(),
        require_auth: false,
        public_url: None,
        active_model: Arc::new(Mutex::new(config.active_model.value.clone())),
        session_service: SessionService::new(),
        session_hub: SessionHub::new(),
        pending_permissions: Arc::new(Mutex::new(HashMap::new())),
        pending_questions: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: Arc::new(Mutex::new(nonoclaw_core::PermissionMode::Default)),
        skills_manager,
        project_service,
        background_registry: Arc::new(std::sync::Mutex::new(
            nonoclaw_tools::BackgroundTaskRegistry::new(),
        )),
        upload_dir,
    })
}

fn token_is_authorized(require_auth: bool, expected: &str, supplied: Option<&str>) -> bool {
    (!require_auth || supplied.is_some())
        && supplied.is_none_or(|token| constant_time_token_eq(expected, token))
}

fn constant_time_token_eq(expected: &str, supplied: &str) -> bool {
    if expected.len() != supplied.len() {
        return false;
    }
    expected
        .as_bytes()
        .iter()
        .zip(supplied.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn listener_requires_auth(addr: &str, tunnel: bool, public_url: Option<&str>) -> bool {
    tunnel
        || public_url.is_some()
        || addr
            .parse::<std::net::SocketAddr>()
            .map(|address| !address.ip().is_loopback())
            .unwrap_or(true)
}

fn list_sessions_wire(state: &AppState) -> Vec<SessionInfoWire> {
    state
        .session_service
        .list_sessions(&state.cwd)
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

// Project, media, and static responsibilities are delegated to their services.

// ── Public entry point ──────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    addr: &str,
    registry: Arc<ToolRegistry>,
    todos: Arc<TodoStore>,
    cwd: PathBuf,
    model: String,
    config: Arc<ResolvedConfig>,
    public_url: Option<String>,
    tunnel: bool,
) -> anyhow::Result<()> {
    // Bind the listener FIRST so the port is open before cloudflared tries
    // to connect (otherwise tunnel spawn races the bind and gets "connection
    // refused" from the OS).
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("NonoClaw web UI listening on http://{addr}");

    // Spawn the tunnel after the port is confirmed open.
    let public_url = if tunnel {
        let tunnel_url = super::static_service::spawn_tunnel(addr).await;
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
    let require_auth = listener_requires_auth(addr, tunnel, public_url.as_deref());
    let active_model = if config
        .conversation_models()
        .iter()
        .any(|profile| profile.name == model)
    {
        model.clone()
    } else {
        config.active_model.value.clone()
    };
    tracing::info!(%active_model, public_auth_required = require_auth, "web authentication policy initialized");

    // File upload storage: ~/.nonoclaw/projects/<cwd>/uploads/
    let upload_dir = nonoclaw_engine::session::home_root()
        .map(|r| {
            r.join("projects")
                .join(
                    cwd.to_string_lossy()
                        .trim_start_matches('/')
                        .replace('/', "-"),
                )
                .join("uploads")
        })
        .unwrap_or_else(|| cwd.join(".nonoclaw/uploads"));
    if let Err(error) = std::fs::create_dir_all(&upload_dir) {
        tracing::warn!(kind = ?error.kind(), "cannot create upload directory");
    }

    let skills_manager = Arc::new(RwLock::new(SkillsManager::new(&cwd)));
    let project_service = Arc::new(ProjectService::new(
        cwd.clone(),
        Arc::clone(&registry),
        Arc::clone(&config),
        public_url.clone(),
        Arc::clone(&skills_manager),
    ));
    let state = Arc::new(AppState {
        config,
        active_model: Arc::new(Mutex::new(active_model)),
        registry,
        todos,
        cwd: cwd.clone(),
        auth_token,
        require_auth,
        public_url,
        session_service: SessionService::new(),
        session_hub: SessionHub::new(),
        pending_permissions: Arc::new(Mutex::new(HashMap::new())),
        pending_questions: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: Arc::new(Mutex::new(nonoclaw_core::PermissionMode::Default)),
        skills_manager,
        project_service,
        background_registry: Arc::new(std::sync::Mutex::new(
            nonoclaw_tools::BackgroundTaskRegistry::new(),
        )),
        upload_dir,
    });

    // Spawn file watcher for hot-reloading skills.
    crate::skill_watcher::spawn_skill_watcher(Arc::clone(&state.skills_manager), cwd.clone());

    // Print only the public origin. The authenticated mobile URL remains
    // available through the in-app QR flow and is never written to logs.
    if let Some(ref url) = state.public_url {
        eprintln!("\n  Tunnel ready: \x1b[1;33m{url}\x1b[0m\n");
    }

    // Always register the WebSocket route + PWA manifest + service worker.
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route(
            "/api/upload",
            axum::routing::post(super::upload_service::upload_handler),
        )
        .route(
            "/api/stt",
            axum::routing::post(super::speech_service::stt_handler),
        )
        .route("/manifest.json", get(super::static_service::serve_manifest))
        .route("/sw.js", get(super::static_service::serve_sw))
        .with_state(state);

    // Optionally serve the built frontend from frontend/dist/.
    let app = if let Some(fe_dir) = super::static_service::frontend_dir(&cwd) {
        let index_path = fe_dir.join("index.html");
        app.route(
            "/",
            axum::routing::get(|| async move { super::static_service::index(index_path).await }),
        )
        .nest_service("/assets", ServeDir::new(fe_dir.join("assets")))
    } else {
        tracing::info!("No frontend/dist found; use Vite dev server for UI");
        app
    };

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

// ── WebSocket handler ───────────────────────────────────────────────────────

async fn ws_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if !state.websocket_authorized(params.get("token").map(String::as_str), peer.ip(), &headers) {
        return super::http_error::error_response(
            StatusCode::UNAUTHORIZED,
            AppError::new(
                ErrorCode::Authentication,
                "invalid or missing auth token",
                false,
                "websocket_authentication",
            )
            .with_trace_id(Uuid::new_v4().to_string()),
        );
    }
    let session_id = params.get("session").cloned();
    ws.on_upgrade(move |socket| handle_ws(socket, state, session_id))
}

async fn handle_ws(ws: WebSocket, state: Arc<AppState>, session_id: Option<String>) {
    let (tx, mut rx) = super::protocol::split_socket(ws);
    let active_controller: Arc<Mutex<Option<RunController>>> = Arc::new(Mutex::new(None));
    // JoinHandle of the current controller supervisor adapter. Cancellation is
    // cooperative through RunController; awaiting this handle guarantees the
    // exactly-once terminal is sent before Clear or a replacement run.
    let mut run_handle: Option<tokio::task::JoinHandle<()>> = None;

    // Capture before any inner shadow (the Run arm destructures session_id).
    // Desktop (no URL param): auto-resume the most recent session.
    // Mobile (QR code): use the session id encoded in the QR.
    let mut shared_sid = session_id.clone().or_else(|| {
        state
            .session_service
            .most_recent_session(&state.cwd)
            .ok()
            .flatten()
    });

    // Register this peer without holding the hub lock across session disk I/O.
    if let Some(ref sid) = shared_sid {
        state
            .session_hub
            .register_existing(&state.session_service, &state.cwd, sid, &tx)
            .await;
    }

    let session: SharedHandle = if let Some(ref sid) = shared_sid {
        state
            .session_hub
            .handle(sid)
            .await
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
        let Some(handle) = existing
            .or_else(|| create_new_session(&state.session_service, &state.cwd, &state.config))
        else {
            send_msg(
                &tx,
                safe_error(
                    ErrorCode::Storage,
                    "session storage is unavailable",
                    true,
                    "session_create",
                ),
            )
            .await;
            return;
        };
        let snapshot = match handle.session.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(_) => {
                send_msg(
                    &tx,
                    safe_error(
                        ErrorCode::Storage,
                        "session snapshot is unavailable",
                        true,
                        "session_snapshot",
                    ),
                )
                .await;
                return;
            }
        };
        let sid = handle.session.id().to_string();
        state
            .session_hub
            .move_registration(shared_sid.as_deref(), &handle, &tx)
            .await;
        shared_sid = Some(sid.clone());
        *session.lock().await = Some(handle);

        send_msg(&tx, messages_loaded(&sid, snapshot)).await;
        send_msg(
            &tx,
            ServerMsg::Info {
                model: state.active_model.lock().await.clone(),
                auth_token: state.auth_token.clone(),
                available_models: state
                    .config
                    .all_models()
                    .iter()
                    .filter(|p| p.is_conversation_model())
                    .map(|p| ModelInfo {
                        name: p.name.clone(),
                        label: p.label.clone().unwrap_or_else(|| p.name.clone()),
                        context_window: p.context_window,
                    })
                    .collect(),
                session_id: sid,
            },
        )
        .await;

        // Send the project file tree so the frontend can render the left rail.
        send_msg(
            &tx,
            ServerMsg::FileTree {
                root: state.cwd.to_string_lossy().to_string(),
                entries: state.project_service.file_tree(),
            },
        )
        .await;

        // Send the full project context for the Insight rail + Git pane.
        let current_model = state.active_model.lock().await.clone();
        let info = state.project_service.snapshot(&current_model).await;
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
                .send(WsMessage::Text(r#"{"type":"ping"}"#.to_string()))
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
            Err(_) => {
                send_msg(
                    &tx,
                    safe_error(
                        ErrorCode::InvalidRequest,
                        "invalid WebSocket message",
                        false,
                        "parse_websocket_message",
                    ),
                )
                .await;
                continue;
            }
        };

        match parsed {
            // ── New / Resume session ────────────────────────────────────────
            ClientMsg::NewSession => {
                let h = create_new_session(&state.session_service, &state.cwd, &state.config);
                match h {
                    Some(h) => {
                        let sid = h.session.id().to_string();
                        let snapshot = match h.session.snapshot().await {
                            Ok(snapshot) => snapshot,
                            Err(_) => {
                                send_msg(
                                    &tx,
                                    safe_error(
                                        ErrorCode::Storage,
                                        "session snapshot is unavailable",
                                        true,
                                        "session_snapshot",
                                    ),
                                )
                                .await;
                                continue;
                            }
                        };
                        send_msg(&tx, messages_loaded(&sid, snapshot)).await;
                        state
                            .session_hub
                            .move_registration(shared_sid.as_deref(), &h, &tx)
                            .await;
                        shared_sid = Some(sid.clone());
                        *session.lock().await = Some(h);
                        send_msg(
                            &tx,
                            ServerMsg::Info {
                                model: state.active_model.lock().await.clone(),
                                auth_token: state.auth_token.clone(),
                                available_models: state
                                    .config
                                    .conversation_models()
                                    .iter()
                                    .map(|p| ModelInfo {
                                        name: p.name.clone(),
                                        label: p.label.clone().unwrap_or_else(|| p.name.clone()),
                                        context_window: p.context_window,
                                    })
                                    .collect(),
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
                            safe_error(
                                ErrorCode::Storage,
                                "session storage is unavailable",
                                true,
                                "session_create",
                            ),
                        )
                        .await;
                    }
                }
            }
            ClientMsg::ResumeSession { id } => {
                match resume_session(&state.session_service, &state.cwd, &id) {
                    Ok(handle) => match handle.session.snapshot().await {
                        Ok(snapshot) => {
                            let sid = handle.session.id().to_string();
                            state
                                .session_hub
                                .move_registration(shared_sid.as_deref(), &handle, &tx)
                                .await;
                            shared_sid = Some(sid.clone());
                            *session.lock().await = Some(handle);
                            send_msg(&tx, messages_loaded(&sid, snapshot)).await;
                            send_msg(
                                &tx,
                                ServerMsg::Info {
                                    model: state.active_model.lock().await.clone(),
                                    auth_token: state.auth_token.clone(),
                                    available_models: state
                                        .config
                                        .all_models()
                                        .iter()
                                        .filter(|profile| profile.is_conversation_model())
                                        .map(|profile| ModelInfo {
                                            name: profile.name.clone(),
                                            label: profile
                                                .label
                                                .clone()
                                                .unwrap_or_else(|| profile.name.clone()),
                                            context_window: profile.context_window,
                                        })
                                        .collect(),
                                    session_id: sid,
                                },
                            )
                            .await;
                        }
                        Err(_) => {
                            send_msg(
                                &tx,
                                safe_error(
                                    ErrorCode::Storage,
                                    "session snapshot is unavailable",
                                    true,
                                    "session_snapshot",
                                ),
                            )
                            .await;
                        }
                    },
                    Err(_) => {
                        send_msg(
                            &tx,
                            safe_error(
                                ErrorCode::NotFound,
                                "session could not be resumed",
                                false,
                                "resume_session",
                            ),
                        )
                        .await;
                    }
                }
            }

            // ── File tree + open-file (frontend left rail) ──────────────────
            ClientMsg::FileTree => {
                send_msg(
                    &tx,
                    ServerMsg::FileTree {
                        root: state.cwd.to_string_lossy().to_string(),
                        entries: state.project_service.file_tree(),
                    },
                )
                .await;
            }
            ClientMsg::ProjectInfoRefresh => {
                let current_model = state.active_model.lock().await.clone();
                let info = state.project_service.refresh(&current_model).await;
                send_msg(&tx, ServerMsg::ProjectInfo { info }).await;
            }
            ClientMsg::GitShow { sha } => match state.project_service.git_show(&sha).await {
                Some(output) => {
                    send_msg(&tx, ServerMsg::GitShow { sha, output }).await;
                }
                None => {
                    send_msg(
                        &tx,
                        safe_error(
                            ErrorCode::NotFound,
                            "commit is invalid or unavailable",
                            false,
                            "git_show",
                        ),
                    )
                    .await;
                }
            },
            ClientMsg::OpenFile { path, force_code } => {
                if state.project_service.open(&path, force_code).is_err() {
                    tracing::warn!(
                        "open-file request denied or failed (path and details redacted)"
                    );
                    send_msg(
                        &tx,
                        safe_error(
                            ErrorCode::PathDenied,
                            "file could not be opened",
                            false,
                            "open_file",
                        ),
                    )
                    .await;
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
                tracing::info!(
                    model = ?model,
                    attachment_count = attachments.as_ref().map_or(0, Vec::len),
                    "ws run request accepted (prompt content omitted)"
                );
                // Cancel and join any in-progress run before taking the next
                // canonical snapshot for this session.
                if let Some(controller) = active_controller.lock().await.as_ref() {
                    controller.cancel("superseded by a new run");
                }
                if let Some(handle) = run_handle.take() {
                    let _ = handle.await;
                }
                *active_controller.lock().await = None;

                let session_for_run = {
                    let guard = session.lock().await;
                    guard.as_ref().map(|handle| handle.session.clone())
                };
                let Some(session_for_run) = session_for_run else {
                    send_msg(
                        &tx,
                        safe_error(
                            ErrorCode::InvalidRequest,
                            "no session is selected",
                            false,
                            "run",
                        ),
                    )
                    .await;
                    continue;
                };
                let session_id = session_for_run.id().to_string();
                let session_snapshot = match session_for_run.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        send_msg(
                            &tx,
                            safe_error(
                                ErrorCode::Storage,
                                "session snapshot is unavailable",
                                true,
                                "session_snapshot",
                            ),
                        )
                        .await;
                        continue;
                    }
                };

                let tx2 = tx.clone();
                let s = state.clone();
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
                    let mut mgr = state.skills_manager.write().unwrap();
                    // Extract skill name from prompt: "/name args..." -> "name"
                    let skill_name = prompt
                        .strip_prefix('/')
                        .and_then(|rest| rest.split_whitespace().next())
                        .unwrap_or("");
                    if !skill_name.is_empty() {
                        if let Some(skill) = mgr.get_skill(skill_name) {
                            if skill.context.as_deref() == Some("fork") {
                                mgr.activate_slash_command(skill_name);
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

                let active_for_run = Arc::clone(&active_controller);
                run_handle = Some(tokio::spawn(async move {
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
                                &s.config,
                                model_used.clone(),
                                None,
                                Some(body.clone()),
                                arguments.clone(),
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
                        let fork_client = match s
                            .config
                            .client_for(ClientPurpose::Subagent, Some(&model_used))
                        {
                            Ok(client) => client,
                            Err(_) => {
                                send_msg(
                                    &tx2,
                                    safe_error(
                                        ErrorCode::Configuration,
                                        "subagent client configuration is invalid",
                                        false,
                                        "build_subagent_client",
                                    ),
                                )
                                .await;
                                return;
                            }
                        };
                        let fork_limits = RunLimits {
                            max_turns: fork_opts.max_turns,
                            max_budget_usd: fork_opts.max_budget_usd,
                            context_window: None,
                        };
                        let fork_engine = QueryEngine::new(
                            fork_client,
                            s.registry.clone(),
                            s.todos.clone(),
                            fork_opts,
                        );
                        let controller = RunController::new(RunContext::new(
                            session_for_run.id(),
                            s.cwd.clone(),
                            model_used.clone(),
                            fork_limits,
                        ));
                        *active_for_run.lock().await = Some(controller.clone());
                        let tx_for_fork_events = tx2.clone();
                        let session_for_fork_events = session_for_run.clone();
                        let completion = controller
                            .start(
                                fork_engine,
                                MessageContent::from_text(&body),
                                move |sequenced| {
                                    let tx_for_fork_events = tx_for_fork_events.clone();
                                    let session_for_fork_events = session_for_fork_events.clone();
                                    async move {
                                        let message =
                                            event_message(&session_for_fork_events, sequenced)
                                                .await;
                                        send_msg(&tx_for_fork_events, message).await;
                                    }
                                },
                            )
                            .wait()
                            .await;
                        let terminal = completion.terminal;
                        let revision = session_for_run
                            .snapshot()
                            .await
                            .map(|snapshot| snapshot.revision)
                            .unwrap_or_default();
                        let (
                            protocol_version,
                            run_id,
                            session_id,
                            session_revision,
                            sequence,
                            timestamp_ms,
                        ) = terminal_fields(&terminal, revision);
                        match terminal.status {
                            RunTerminalStatus::Done => {
                                if let Some(result) = terminal.result {
                                    send_msg(
                                        &tx2,
                                        ServerMsg::Done {
                                            protocol_version,
                                            run_id,
                                            session_id,
                                            session_revision,
                                            sequence,
                                            timestamp_ms,
                                            text: result.text,
                                            usage: serde_json::to_value(result.usage)
                                                .unwrap_or_default(),
                                            turns: result.turns,
                                            stop_reason: result
                                                .stop_reason
                                                .as_ref()
                                                .map(|s| s.as_str().to_string()),
                                        },
                                    )
                                    .await;
                                } else {
                                    send_msg(
                                        &tx2,
                                        ServerMsg::RunError {
                                            protocol_version,
                                            run_id,
                                            session_id,
                                            session_revision,
                                            sequence,
                                            timestamp_ms,
                                            error: AppError::new(
                                                ErrorCode::Internal,
                                                "fork run completed without a result",
                                                false,
                                                "fork_run",
                                            )
                                            .with_trace_id(Uuid::new_v4().to_string()),
                                        },
                                    )
                                    .await;
                                }
                            }
                            RunTerminalStatus::Cancelled => {
                                send_msg(
                                    &tx2,
                                    ServerMsg::Done {
                                        protocol_version,
                                        run_id,
                                        session_id,
                                        session_revision,
                                        sequence,
                                        timestamp_ms,
                                        text: "Run cancelled.".into(),
                                        usage: serde_json::json!({}),
                                        turns: 0,
                                        stop_reason: Some("cancelled".into()),
                                    },
                                )
                                .await;
                            }
                            RunTerminalStatus::Error => {
                                send_msg(
                                    &tx2,
                                    ServerMsg::RunError {
                                        protocol_version,
                                        run_id,
                                        session_id,
                                        session_revision,
                                        sequence,
                                        timestamp_ms,
                                        error: AppError::new(
                                            ErrorCode::Internal,
                                            "fork execution failed",
                                            false,
                                            "fork_run",
                                        )
                                        .with_trace_id(Uuid::new_v4().to_string()),
                                    },
                                )
                                .await;
                            }
                        }
                        *active_for_run.lock().await = None;
                        return;
                    }

                    let mut options = build_options(
                        &s.config,
                        model_used.clone(),
                        max_turns,
                        append_system_prompt.clone(),
                        arguments.clone(),
                        tx2.clone(),
                        Arc::clone(&s.pending_permissions),
                        *s.permission_mode.lock().await,
                        Arc::clone(&s.skills_manager),
                        Arc::clone(&s.background_registry),
                    );

                    // Question resolver (per-run to avoid oneshot key clashes).
                    let qr: Arc<dyn QuestionResolver> = Arc::new(WsQuestionResolver {
                        request_id: format!("{request_id}-q"),
                        pending: Arc::clone(&s.pending_questions),
                        tx: tx2.clone(),
                    });
                    options.question_resolver = Some(qr);

                    // Resolve credentials/format from the same immutable
                    // snapshot. No process environment is changed when a Web
                    // session selects a different model.
                    let run_client = match s
                        .config
                        .client_for(ClientPurpose::Conversation, Some(&model_used))
                    {
                        Ok(client) => client,
                        Err(_) => {
                            tracing::warn!(model = %model_used, "resolved run client build failed (details redacted)");
                            send_msg(
                                &tx2,
                                safe_error(
                                    ErrorCode::Configuration,
                                    "model client configuration is invalid",
                                    false,
                                    "build_model_client",
                                ),
                            )
                            .await;
                            return;
                        }
                    };

                    let session_for_wire = session_for_run.clone();
                    let engine = QueryEngine::with_session(
                        run_client,
                        s.registry.clone(),
                        s.todos.clone(),
                        options,
                        session_for_run,
                        session_snapshot,
                    );

                    // Enrich the prompt with attachment content + images.
                    let enriched =
                        enrich_prompt_with_attachments(&prompt, &attachments, &s.upload_dir);
                    let controller = RunController::for_engine(&engine, s.cwd.clone());
                    *active_for_run.lock().await = Some(controller.clone());

                    tracing::debug!(
                        "starting engine run (attachments: {})",
                        attachments.as_ref().map(|a| a.len()).unwrap_or(0)
                    );
                    let tx_for_events = tx2.clone();
                    let session_for_events = session_for_wire.clone();
                    let completion = controller
                        .start(engine, enriched, move |sequenced| {
                            let tx_for_events = tx_for_events.clone();
                            let session_for_events = session_for_events.clone();
                            async move {
                                tracing::debug!(
                                    run_id = %sequenced.run_id,
                                    sequence = sequenced.sequence,
                                    "engine event emitted (payload omitted)"
                                );
                                let message = event_message(&session_for_events, sequenced).await;
                                send_msg(&tx_for_events, message).await;
                            }
                        })
                        .wait()
                        .await;

                    let terminal = completion.terminal;
                    let revision = session_for_wire
                        .snapshot()
                        .await
                        .map(|snapshot| snapshot.revision)
                        .unwrap_or_default();
                    let (
                        protocol_version,
                        run_id,
                        session_id,
                        session_revision,
                        sequence,
                        timestamp_ms,
                    ) = terminal_fields(&terminal, revision);
                    match terminal.status {
                        RunTerminalStatus::Done => {
                            let Some(r) = terminal.result else {
                                send_msg(
                                    &tx2,
                                    ServerMsg::RunError {
                                        protocol_version,
                                        run_id,
                                        session_id,
                                        session_revision,
                                        sequence,
                                        timestamp_ms,
                                        error: AppError::new(
                                            ErrorCode::Internal,
                                            "run completed without a result",
                                            false,
                                            "run",
                                        )
                                        .with_trace_id(Uuid::new_v4().to_string()),
                                    },
                                )
                                .await;
                                *active_for_run.lock().await = None;
                                return;
                            };
                            tracing::info!(
                                turns = r.turns,
                                text_len = r.text.len(),
                                "engine run complete"
                            );
                            let msg = ServerMsg::Done {
                                protocol_version,
                                run_id,
                                session_id,
                                session_revision,
                                sequence,
                                timestamp_ms,
                                text: r.text,
                                usage: serde_json::to_value(r.usage).unwrap_or_default(),
                                turns: r.turns,
                                stop_reason: r.stop_reason.as_ref().map(|s| s.as_str().to_string()),
                            };
                            send_msg(&tx2, msg).await;

                            // Refresh project context: git status / files may
                            // have changed after the run.
                            let current_model = s.active_model.lock().await.clone();
                            let info = s.project_service.snapshot(&current_model).await;
                            send_msg(&tx2, ServerMsg::ProjectInfo { info: info.clone() }).await;

                            // Broadcast updated messages + project info to all
                            // other peers sharing this session.
                            if let Some(ref cid) = sync_sid {
                                s.session_hub.sync(cid, &tx2).await;
                                // Also push ProjectInfo refresh.
                                let pi = ServerMsg::ProjectInfo { info };
                                let peers = s.session_hub.peers(cid).await;
                                let mut dead = Vec::new();
                                for peer in peers {
                                    if Arc::ptr_eq(&peer, &tx2) {
                                        continue;
                                    }
                                    if !send_msg_ok(&peer, &pi).await {
                                        dead.push(peer);
                                    }
                                }
                                if !dead.is_empty() {
                                    s.session_hub.remove_dead(cid, &dead).await;
                                }
                            }
                        }
                        RunTerminalStatus::Cancelled => {
                            send_msg(
                                &tx2,
                                ServerMsg::Done {
                                    protocol_version,
                                    run_id,
                                    session_id,
                                    session_revision,
                                    sequence,
                                    timestamp_ms,
                                    text: "Run cancelled.".into(),
                                    usage: serde_json::json!({}),
                                    turns: 0,
                                    stop_reason: Some("cancelled".into()),
                                },
                            )
                            .await;
                        }
                        RunTerminalStatus::Error => {
                            tracing::error!(run_id = %run_id, "engine run failed (details redacted)");
                            send_msg(
                                &tx2,
                                ServerMsg::RunError {
                                    protocol_version,
                                    run_id,
                                    session_id,
                                    session_revision,
                                    sequence,
                                    timestamp_ms,
                                    error: AppError::new(
                                        ErrorCode::Internal,
                                        "run failed",
                                        false,
                                        "run",
                                    )
                                    .with_trace_id(Uuid::new_v4().to_string()),
                                },
                            )
                            .await;
                        }
                    }
                    *active_for_run.lock().await = None;
                }));
            }

            // ── Cancel ──────────────────────────────────────────────────────
            ClientMsg::Cancel => {
                if let Some(controller) = active_controller.lock().await.as_ref() {
                    controller.cancel("user requested cancellation");
                }
                if let Some(handle) = run_handle.take() {
                    let _ = handle.await;
                }
                *active_controller.lock().await = None;
            }

            // ── Switch permission mode at runtime ──────────────────────────
            ClientMsg::SetPermissionMode { mode } => {
                let new_mode = match mode.as_str() {
                    "auto" => nonoclaw_core::PermissionMode::Auto,
                    "bypass" | "bypassPermissions" => {
                        nonoclaw_core::PermissionMode::BypassPermissions
                    }
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
                if state.config.all_models().iter().any(|p| p.name == name) {
                    // Only session state changes. Client credentials are derived
                    // per run from ResolvedConfig, avoiding process-wide races.
                    *state.active_model.lock().await = name.clone();
                    tracing::info!(%name, "active model switched");
                    // Push updated Info + ProjectInfo so the UI reflects the new model immediately.
                    send_msg(
                        &tx,
                        ServerMsg::Info {
                            model: name.clone(),
                            auth_token: state.auth_token.clone(),
                            available_models: state
                                .config
                                .all_models()
                                .iter()
                                .filter(|p| p.is_conversation_model())
                                .map(|p| ModelInfo {
                                    name: p.name.clone(),
                                    label: p.label.clone().unwrap_or_else(|| p.name.clone()),
                                    context_window: p.context_window,
                                })
                                .collect(),
                            session_id: session
                                .lock()
                                .await
                                .as_ref()
                                .map(|handle| handle.session.id().to_string())
                                .unwrap_or_default(),
                        },
                    )
                    .await;
                    let info = state.project_service.snapshot(&name).await;
                    send_msg(&tx, ServerMsg::ProjectInfo { info }).await;
                } else {
                    tracing::warn!("unknown model requested — ignored");
                }
            }

            // ── Clear (in-memory only; on-disk transcript is the archive) ───
            ClientMsg::Clear => {
                // Stop and join the event source before publishing the empty
                // transcript, so no stale event can follow MessagesLoaded.
                if let Some(controller) = active_controller.lock().await.as_ref() {
                    controller.cancel("session cleared");
                }
                if let Some(handle) = run_handle.take() {
                    let _ = handle.await;
                }
                *active_controller.lock().await = None;
                // Submit the clear as one atomic writer command.
                let canonical = session
                    .lock()
                    .await
                    .as_ref()
                    .map(|handle| handle.session.clone());
                if let Some(canonical) = canonical {
                    if canonical.clear().await.is_err() {
                        tracing::warn!("failed to clear session (details redacted)");
                        send_msg(
                            &tx,
                            safe_error(
                                ErrorCode::Storage,
                                "session could not be cleared",
                                true,
                                "clear_session",
                            ),
                        )
                        .await;
                        continue;
                    }
                    match canonical.snapshot().await {
                        Ok(snapshot) => {
                            let ml = messages_loaded(canonical.id(), snapshot);
                            send_msg(&tx, ml).await;
                        }
                        Err(_) => {
                            send_msg(
                                &tx,
                                safe_error(
                                    ErrorCode::Storage,
                                    "session snapshot is unavailable",
                                    true,
                                    "session_snapshot",
                                ),
                            )
                            .await;
                            continue;
                        }
                    }
                }

                // Broadcast the clear to all other peers.
                if let Some(ref cid) = shared_sid {
                    state.session_hub.sync(cid, &tx).await;
                }
            }

            // ── Manual /compact ─────────────────────────────────────────────
            ClientMsg::Compact => {
                if let Some(controller) = active_controller.lock().await.as_ref() {
                    controller.cancel("manual compaction requested");
                }
                if let Some(handle) = run_handle.take() {
                    let _ = handle.await;
                }
                *active_controller.lock().await = None;

                let canonical = session
                    .lock()
                    .await
                    .as_ref()
                    .map(|handle| handle.session.clone());
                let Some(canonical) = canonical else {
                    send_msg(
                        &tx,
                        safe_error(
                            ErrorCode::InvalidRequest,
                            "no session is selected",
                            false,
                            "compact_session",
                        ),
                    )
                    .await;
                    continue;
                };
                let snapshot = match canonical.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        send_msg(
                            &tx,
                            safe_error(
                                ErrorCode::Storage,
                                "session snapshot is unavailable",
                                true,
                                "session_snapshot",
                            ),
                        )
                        .await;
                        continue;
                    }
                };
                let original_count = snapshot.messages.len();
                let compact_run_id = Uuid::new_v4().to_string();
                let compact_session_id = canonical.id().to_string();
                let compact_start_revision = snapshot.revision;
                send_msg(
                    &tx,
                    synthetic_event_message(
                        &compact_run_id,
                        &compact_session_id,
                        compact_start_revision,
                        1,
                        RunEvent::Compacting,
                    ),
                )
                .await;
                let compact_for_model = state.active_model.lock().await.clone();
                let options = build_options(
                    &state.config,
                    compact_for_model.clone(),
                    None,
                    None,
                    None,
                    tx.clone(),
                    Arc::clone(&state.pending_permissions),
                    *state.permission_mode.lock().await,
                    Arc::clone(&state.skills_manager),
                    Arc::clone(&state.background_registry),
                );
                let compact_client = match state
                    .config
                    .client_for(ClientPurpose::Conversation, Some(&compact_for_model))
                {
                    Ok(client) => client,
                    Err(_) => {
                        send_msg(
                            &tx,
                            safe_error(
                                ErrorCode::Configuration,
                                "model client configuration is invalid",
                                false,
                                "build_model_client",
                            ),
                        )
                        .await;
                        continue;
                    }
                };
                let session_after_compact = canonical.clone();
                let mut engine = QueryEngine::with_session(
                    compact_client,
                    state.registry.clone(),
                    state.todos.clone(),
                    options,
                    canonical,
                    snapshot,
                );
                match engine.compact_now().await {
                    Ok(Some((removed, kept))) => {
                        let revision = session_after_compact
                            .snapshot()
                            .await
                            .map(|snapshot| snapshot.revision)
                            .unwrap_or(compact_start_revision);
                        send_msg(
                            &tx,
                            synthetic_event_message(
                                &compact_run_id,
                                &compact_session_id,
                                revision,
                                2,
                                RunEvent::Compacted {
                                    removed,
                                    kept,
                                    tokens_before: 0,
                                    tokens_after: 0,
                                },
                            ),
                        )
                        .await;
                        if let Some(ref id) = shared_sid {
                            state.session_hub.sync(id, &tx).await;
                        }
                    }
                    Ok(None) => {
                        send_msg(
                            &tx,
                            synthetic_event_message(
                                &compact_run_id,
                                &compact_session_id,
                                compact_start_revision,
                                2,
                                RunEvent::Compacted {
                                    removed: 0,
                                    kept: original_count,
                                    tokens_before: 0,
                                    tokens_after: 0,
                                },
                            ),
                        )
                        .await;
                    }
                    Err(_) => {
                        send_msg(
                            &tx,
                            safe_error(
                                ErrorCode::Internal,
                                "session compaction failed",
                                true,
                                "compact_session",
                            ),
                        )
                        .await;
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
            ClientMsg::QuestionAnswer { request_id, answer } => {
                let sender = state.pending_questions.lock().await.remove(&request_id);
                if let Some(sender) = sender {
                    let _ = sender.send(answer);
                }
            }
        }
    }
    // The connection owns its active run. Disconnecting cancels the complete
    // tree (provider stream, tools, child agents, and event consumer).
    if let Some(controller) = active_controller.lock().await.as_ref() {
        controller.cancel("websocket disconnected");
    }
    if let Some(handle) = run_handle.take() {
        let _ = handle.await;
    }
    // Loop exited — stop the keepalive pinger for this connection.
    ping_handle.abort();
    // Connection closed — remove this Tx from the shared session's broadcast
    // list so we don't keep trying to send to a dead peer.
    if let Some(ref sid) = shared_sid {
        state.session_hub.disconnect(sid, &tx).await;
    }
}

#[cfg(test)]
mod characterization_tests {
    use super::*;

    #[test]
    fn public_token_policy_keeps_loopback_low_friction() {
        assert!(token_is_authorized(false, "secret", None));
        assert!(token_is_authorized(true, "secret", Some("secret")));
        assert!(!token_is_authorized(true, "secret", None));
        assert!(!token_is_authorized(false, "secret", Some("wrong")));
    }

    #[test]
    fn listener_auth_policy_covers_loopback_public_tunnel_and_invalid_addresses() {
        // **Validates: Requirements 11.2**
        assert!(!listener_requires_auth("127.0.0.1:3000", false, None));
        assert!(!listener_requires_auth("[::1]:3000", false, None));
        assert!(listener_requires_auth("0.0.0.0:3000", false, None));
        assert!(listener_requires_auth("192.0.2.10:3000", false, None));
        assert!(listener_requires_auth("127.0.0.1:3000", true, None));
        assert!(listener_requires_auth(
            "127.0.0.1:3000",
            false,
            Some("https://public.example")
        ));
        assert!(listener_requires_auth("not-an-address", false, None));
    }

    #[test]
    fn websocket_auth_policy_allows_only_direct_loopback_bootstrap_without_token() {
        let loopback_v4: IpAddr = "127.0.0.1".parse().unwrap();
        let loopback_v6: IpAddr = "::1".parse().unwrap();
        let remote: IpAddr = "192.0.2.10".parse().unwrap();

        assert!(!request_requires_auth(true, loopback_v4, false));
        assert!(!request_requires_auth(true, loopback_v6, false));
        assert!(request_requires_auth(true, loopback_v4, true));
        assert!(request_requires_auth(true, remote, false));
        assert!(!request_requires_auth(false, remote, true));

        assert!(token_is_authorized(false, "secret", None));
        assert!(!token_is_authorized(false, "secret", Some("wrong")));
        assert!(!token_is_authorized(true, "secret", None));
        assert!(token_is_authorized(true, "secret", Some("secret")));
    }

    fn client_kind(message: ClientMsg) -> &'static str {
        match message {
            ClientMsg::Run { .. } => "run",
            ClientMsg::Cancel => "cancel",
            ClientMsg::Clear => "clear",
            ClientMsg::NewSession => "new_session",
            ClientMsg::ResumeSession { .. } => "resume_session",
            ClientMsg::Compact => "compact",
            ClientMsg::PermissionDecision { .. } => "permission_decision",
            ClientMsg::QuestionAnswer { .. } => "question_answer",
            ClientMsg::FileTree => "file_tree",
            ClientMsg::OpenFile { .. } => "open_file",
            ClientMsg::ProjectInfoRefresh => "project_info_refresh",
            ClientMsg::GitShow { .. } => "git_show",
            ClientMsg::SetPermissionMode { .. } => "set_permission_mode",
            ClientMsg::SetModel { .. } => "set_model",
        }
    }

    /// Checked fixtures for every browser-to-server message plus the minimal
    /// run → event → done Web success path. Feature Preservation Matrix: §4.2-4.4.
    #[test]
    fn websocket_protocol_and_web_success_path_are_stable() {
        let fixtures = [
            (
                r#"{"type":"run","prompt":"hello","model":"fixture-model","max_turns":1,"append_system_prompt":"extra","arguments":"arg","attachments":[{"id":"a","filename":"a.txt","extracted_text":"body","images":[]}]}"#,
                "run",
            ),
            (r#"{"type":"cancel"}"#, "cancel"),
            (r#"{"type":"clear"}"#, "clear"),
            (r#"{"type":"new_session"}"#, "new_session"),
            (
                r#"{"type":"resume_session","id":"abc-123"}"#,
                "resume_session",
            ),
            (r#"{"type":"compact"}"#, "compact"),
            (
                r#"{"type":"permission_decision","request_id":"p1","decision":"allow"}"#,
                "permission_decision",
            ),
            (
                r#"{"type":"question_answer","request_id":"q1","answer":"yes"}"#,
                "question_answer",
            ),
            (r#"{"type":"file_tree"}"#, "file_tree"),
            (
                r#"{"type":"open_file","path":"src/main.rs","force_code":true}"#,
                "open_file",
            ),
            (r#"{"type":"project_info_refresh"}"#, "project_info_refresh"),
            (r#"{"type":"git_show","sha":"abc123"}"#, "git_show"),
            (
                r#"{"type":"set_permission_mode","mode":"plan"}"#,
                "set_permission_mode",
            ),
            (
                r#"{"type":"set_model","name":"fixture-model"}"#,
                "set_model",
            ),
        ];
        for (json, expected) in fixtures {
            let parsed: ClientMsg = serde_json::from_str(json).unwrap();
            assert_eq!(client_kind(parsed), expected);
        }

        let event = ServerMsg::Event {
            envelope: EventEnvelope::at(
                "run-fixture",
                None,
                "session-fixture",
                7,
                3,
                1_700_000_000_000,
                EngineEvent::TextDelta {
                    text: "fixture answer".into(),
                },
            ),
        };
        let done = ServerMsg::Done {
            protocol_version: WS_PROTOCOL_VERSION,
            run_id: "run-fixture".into(),
            session_id: "session-fixture".into(),
            session_revision: 8,
            sequence: 4,
            timestamp_ms: 1_700_000_000_002,
            text: "fixture answer".into(),
            usage: serde_json::json!({
                "input_tokens": 1,
                "output_tokens": 2,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }),
            turns: 1,
            stop_reason: Some("end_turn".into()),
        };
        let event_json = serde_json::to_value(event).unwrap();
        let done_json = serde_json::to_value(done).unwrap();
        assert_eq!(event_json["type"], "event");
        assert_eq!(event_json["protocol_version"], WS_PROTOCOL_VERSION);
        assert_eq!(event_json["run_id"], "run-fixture");
        assert_eq!(event_json["session_id"], "session-fixture");
        assert_eq!(event_json["session_revision"], 7);
        assert_eq!(event_json["sequence"], 3);
        assert_eq!(event_json["event"]["kind"], "text_delta");
        assert_eq!(event_json["event"]["text"], "fixture answer");
        assert_eq!(done_json["type"], "done");
        assert_eq!(done_json["text"], "fixture answer");
    }

    /// Ensures every current server-to-browser tag remains serializable.
    #[test]
    fn websocket_server_message_tags_are_stable() {
        let project_info = ProjectInfo {
            cwd: "/fixture".into(),
            model: "fixture-model".into(),
            tools: vec![],
            mcp_servers: vec![],
            skills: vec![],
            plugins: vec![],
            extensions: vec![],
            extension_diagnostics: vec![],
            hooks: vec![],
            docs: vec![],
            settings: vec![],
            cli_reference: vec![],
            config_reference: vec![],
            config_diagnostics: vec![],
            git: None,
            context_window: None,
            compact_threshold: 80_000,
            public_url: None,
        };
        let messages = vec![
            ServerMsg::Event {
                envelope: EventEnvelope::at(
                    "r",
                    None,
                    "s",
                    1,
                    1,
                    1,
                    EngineEvent::AssistantDone { text: "ok".into() },
                ),
            },
            ServerMsg::PermissionRequired {
                request_id: "p".into(),
                tool_name: "Write".into(),
                message: "allow?".into(),
                input: serde_json::json!({}),
            },
            ServerMsg::QuestionRequired {
                request_id: "q".into(),
                prompt: "choose".into(),
                options: vec!["a".into()],
            },
            ServerMsg::Done {
                protocol_version: WS_PROTOCOL_VERSION,
                run_id: "r".into(),
                session_id: "s".into(),
                session_revision: 1,
                sequence: 2,
                timestamp_ms: 2,
                text: "ok".into(),
                usage: serde_json::json!({}),
                turns: 1,
                stop_reason: None,
            },
            ServerMsg::Error {
                error: AppError::new(ErrorCode::Internal, "error", false, "fixture")
                    .with_trace_id("trace-fixture"),
            },
            ServerMsg::Info {
                model: "m".into(),
                session_id: "s".into(),
                auth_token: "t".into(),
                available_models: vec![],
            },
            ServerMsg::SessionList { sessions: vec![] },
            ServerMsg::MessagesLoaded {
                protocol_version: WS_PROTOCOL_VERSION,
                session_id: "s".into(),
                revision: 1,
                timestamp_ms: 1,
                messages: vec![],
            },
            ServerMsg::FileTree {
                root: "/fixture".into(),
                entries: vec![],
            },
            ServerMsg::ProjectInfo { info: project_info },
            ServerMsg::GitShow {
                sha: "abc".into(),
                output: "patch".into(),
            },
        ];
        let tags: Vec<_> = messages
            .into_iter()
            .map(|message| {
                serde_json::to_value(message).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            tags,
            [
                "event",
                "permission_required",
                "question_required",
                "done",
                "error",
                "info",
                "session_list",
                "messages_loaded",
                "file_tree",
                "project_info",
                "git_show"
            ]
        );
    }
}
