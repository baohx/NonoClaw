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

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::{
    extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    extract::{ConnectInfo, DefaultBodyLimit, Query, State},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use nonoclaw_api::ClientConfig;
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

#[allow(unused_imports)]
use super::protocol::history_page;
#[cfg(test)]
use super::protocol::WS_PROTOCOL_VERSION;
use super::protocol::{
    event_message, messages_loaded, safe_error, send_msg, send_msg_ok, synthetic_event_message,
    terminal_fields, ClientMsg, ModelInfo, ServerMsg, SessionInfoWire,
};
use crate::attachments;
#[cfg(test)]
use crate::project_info::ProjectInfo;
#[cfg(test)]
use nonoclaw_engine::{EngineEvent, EventEnvelope};

use super::http_error::model_client_error;
use super::permission_api::PendingPermissionMeta;
use super::project_context::{upload_dir_for, ProjectContext, ProjectContextStore};
use super::project_service::ProjectService;
use super::run_handler::{
    build_options, enrich_prompt_with_attachments, PermissionMap, QuestionMap, WsQuestionResolver,
};
use super::session_hub::{
    create_new_session, resume_session, CancelRunResult, SessionHub, SharedHandle,
};

fn safe_provider_failure_message(reason: &str, status: Option<u16>) -> String {
    if reason == "provider request failed" {
        if let Some(status) = status {
            return format!("provider request failed (HTTP {status})");
        }
    }
    reason.to_string()
}

// ── Shared application state ────────────────────────────────────────────────

const LOCAL_AUTH_COOKIE: &str = "nonoclaw_local_ticket";
const LOCAL_BOOTSTRAP_HEADER: &str = "x-nonoclaw-bootstrap";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebSocketAuthKind {
    /// A same-origin browser on the machine running NonoClaw. The local
    /// ticket is HttpOnly and is never exposed to JavaScript.
    LocalTicket,
    /// A remote/mobile or non-browser client that supplied the QR token.
    RemoteToken,
}

pub(super) struct AppState {
    pub(super) registry: Arc<ToolRegistry>,
    pub(super) todos: Arc<TodoStore>,
    /// Atomically published cwd/config/skills/upload snapshot. Every operation
    /// clones one context so project switching cannot mix old and new fields.
    pub(super) projects: super::project_context::ProjectContextStore,
    /// Auth token for remote (QR-code) mobile access.
    auth_token: String,
    /// Independent browser bootstrap ticket. It is delivered only as an
    /// HttpOnly, SameSite=Strict cookie to a direct loopback same-origin page.
    local_ws_ticket: String,
    /// Public/tunnel listeners require the remote token for legacy REST paths.
    require_auth: bool,
    /// The public URL shown in the QR code, or None.
    public_url: Option<String>,
    /// Currently active model name (switchable via SetModel client message + UI).
    pub(super) active_model: Arc<Mutex<String>>,
    /// Canonical owner for session discovery and per-session writer actors.
    pub(super) session_service: SessionService,
    /// Session peer registration and revisioned broadcast owner.
    pub(super) session_hub: SessionHub,
    pub(super) pending_permissions: Arc<PermissionMap>,
    pub(super) pending_questions: Arc<QuestionMap>,
    /// Metadata for pending permission requests (for REST API inspection).
    pub(super) permission_meta: PendingPermissionMeta,
    /// Metadata for pending questions (Factor 7 REST API).
    pub(super) question_meta: super::permission_api::PendingQuestionMeta,
    /// Runtime-mutable permission mode (switchable via UI).
    pub(super) permission_mode: Arc<Mutex<nonoclaw_core::PermissionMode>>,
    /// Deduplicated owner of git/config/skills ProjectInfo and file operations.
    project_service: Arc<ProjectService>,
    /// Background task registry for run_in_background bash commands.
    pub(super) background_registry: Arc<std::sync::Mutex<nonoclaw_tools::BackgroundTaskRegistry>>,
    /// Resolved path to the `markitdown` CLI, from the runtime probe.
    /// `None` before the probe completes or when MarkItDown is absent.
    /// Updated atomically by the probe updater via `Arc<Mutex<..>>`.
    pub(super) markitdown_path: Arc<Mutex<Option<String>>>,
    /// Last observed client activity (AutoDream idle detection).
    pub(super) last_activity: Arc<Mutex<std::time::SystemTime>>,
    /// Video generation local queue + Ark RPM window.
    pub(super) video_queue: Arc<super::video_service::VideoStore>,
}

impl AppState {
    pub(super) fn project(&self) -> Arc<super::project_context::ProjectContext> {
        self.projects.snapshot()
    }

    pub(super) fn cwd(&self) -> PathBuf {
        self.project().cwd().to_path_buf()
    }

    pub(super) fn authorized(&self, supplied_token: Option<&str>) -> bool {
        token_is_authorized(self.require_auth, &self.auth_token, supplied_token)
    }

    pub(super) fn download_authorized(&self, supplied_token: Option<&str>) -> bool {
        supplied_token.is_some_and(|token| constant_time_token_eq(&self.auth_token, token))
    }

    /// Authenticate a state-changing REST control endpoint. Unlike legacy
    /// media/log endpoints this is always required, including on loopback.
    /// Local browser calls use the HttpOnly ticket; automation uses a Bearer
    /// or query token.
    pub(super) fn control_authorized(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> bool {
        query_token.is_some_and(|token| constant_time_token_eq(&self.auth_token, token))
            || bearer_token(headers)
                .is_some_and(|token| constant_time_token_eq(&self.auth_token, token))
            || cookie_token(headers, LOCAL_AUTH_COOKIE)
                .is_some_and(|token| constant_time_token_eq(&self.local_ws_ticket, token))
    }

    /// Path for persisting pending permission metadata across restarts.
    fn pending_permissions_path(&self) -> Option<std::path::PathBuf> {
        nonoclaw_engine::session::project_dir(&self.cwd())
            .map(|d| d.join("pending_permissions.json"))
    }

    /// Persist current pending permissions to disk (best-effort).
    pub(super) async fn persist_pending_permissions(&self) {
        let Some(path) = self.pending_permissions_path() else {
            return;
        };
        let metas = self.permission_meta.lock().await;
        let entries: Vec<_> = metas.values().cloned().collect();
        drop(metas);
        if let Err(e) = std::fs::write(&path, serde_json::to_vec(&entries).unwrap_or_default()) {
            tracing::warn!(kind = ?e.kind(), "failed to persist pending permissions");
        }
    }

    /// Load persisted pending permissions from disk (best-effort).
    pub(super) async fn load_pending_permissions(&self) {
        let Some(path) = self.pending_permissions_path() else {
            return;
        };
        let Ok(data) = std::fs::read(&path) else {
            return;
        };
        let Ok(entries) =
            serde_json::from_slice::<Vec<super::permission_api::PendingPermissionInfo>>(&data)
        else {
            return;
        };
        if entries.is_empty() {
            // Clean up stale file.
            let _ = std::fs::remove_file(&path);
            return;
        }
        // Load metadata entries — the oneshot senders are gone, but the
        // metadata is useful for audit/debugging. REST resolution will return
        // 410 Gone (run no longer active) which is correct behavior.
        let mut metas = self.permission_meta.lock().await;
        for entry in entries {
            if super::session_hub::valid_session_id(&entry.session_id) {
                let key = (entry.session_id.clone(), entry.request_id.clone());
                metas.insert(key, entry);
            }
        }
        tracing::info!(count = metas.len(), "loaded persisted pending permissions");
    }

    fn websocket_authorization(
        &self,
        supplied_token: Option<&str>,
        peer_ip: IpAddr,
        headers: &HeaderMap,
    ) -> Option<WebSocketAuthKind> {
        // Explicit QR/Bearer-style launch tokens support remote browsers and
        // non-browser clients. Browsers must still be same-origin with Host;
        // clients without Origin are permitted only because possession of the
        // high-entropy token is their authentication boundary.
        if supplied_token.is_some_and(|token| constant_time_token_eq(&self.auth_token, token))
            && browser_origin_allowed(headers, false)
        {
            return Some(WebSocketAuthKind::RemoteToken);
        }

        // The local ticket is intentionally narrower: direct loopback only,
        // no reverse-proxy headers, a loopback Host, and an exact Origin/Host
        // match. This blocks cross-site WebSocket hijacking and DNS rebinding.
        if cookie_token(headers, LOCAL_AUTH_COOKIE)
            .is_some_and(|token| constant_time_token_eq(&self.local_ws_ticket, token))
            && local_browser_request_allowed(peer_ip, headers, true)
        {
            return Some(WebSocketAuthKind::LocalTicket);
        }

        None
    }
}

fn has_forwarding_headers(headers: &HeaderMap) -> bool {
    headers.contains_key("forwarded")
        || headers.contains_key("x-forwarded-for")
        || headers.contains_key("x-real-ip")
        || headers.contains_key("cf-connecting-ip")
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)?
                .to_str()
                .ok()?
                .strip_prefix("bearer ")
        })
}

fn cookie_token<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn normalized_authority(url: &reqwest::Url) -> Option<(String, u16)> {
    let host = url
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    Some((host, url.port_or_known_default()?))
}

fn origin_matches_host(headers: &HeaderMap) -> bool {
    let origin = match headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        Some(origin) if origin != "null" => origin,
        _ => return false,
    };
    let host = match headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    {
        Some(host) => host,
        None => return false,
    };
    let Ok(origin_url) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(origin_url.scheme(), "http" | "https") {
        return false;
    }
    let Ok(host_url) = reqwest::Url::parse(&format!("{}://{host}", origin_url.scheme())) else {
        return false;
    };
    normalized_authority(&origin_url) == normalized_authority(&host_url)
}

fn is_loopback_hostname(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn host_is_loopback(headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(url) = reqwest::Url::parse(&format!("http://{host}")) else {
        return false;
    };
    url.host_str().is_some_and(is_loopback_hostname)
}

fn request_declares_cross_site(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
}

fn browser_origin_allowed(headers: &HeaderMap, origin_required: bool) -> bool {
    if request_declares_cross_site(headers) {
        return false;
    }
    if !headers.contains_key(header::ORIGIN) {
        return !origin_required;
    }
    origin_matches_host(headers)
}

fn local_browser_request_allowed(
    peer_ip: IpAddr,
    headers: &HeaderMap,
    origin_required: bool,
) -> bool {
    peer_ip.is_loopback()
        && !has_forwarding_headers(headers)
        && host_is_loopback(headers)
        && browser_origin_allowed(headers, origin_required)
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
    let projects = ProjectContextStore::new(ProjectContext::new(
        1,
        cwd,
        Arc::clone(&config),
        skills_manager,
        upload_dir,
    ));
    let project_service = Arc::new(ProjectService::new(Arc::clone(&registry), None));
    Arc::new(AppState {
        registry,
        todos,
        projects,
        auth_token: "exploration-token".into(),
        local_ws_ticket: "exploration-local-ticket".into(),
        require_auth: false,
        public_url: None,
        active_model: Arc::new(Mutex::new(config.active_model.value.clone())),
        session_service: SessionService::new(),
        session_hub: SessionHub::new(),
        pending_permissions: Arc::new(Mutex::new(HashMap::new())),
        pending_questions: Arc::new(Mutex::new(HashMap::new())),
        permission_meta: Arc::new(Mutex::new(HashMap::new())),
        question_meta: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: Arc::new(Mutex::new(initial_permission_mode(&config))),
        project_service,
        background_registry: Arc::new(std::sync::Mutex::new(
            nonoclaw_tools::BackgroundTaskRegistry::new(),
        )),
        markitdown_path: Arc::new(Mutex::new(None)),
        last_activity: Arc::new(Mutex::new(std::time::SystemTime::now())),
        video_queue: Arc::new(super::video_service::VideoStore::new()),
    })
}

/// Resolve the initial permission mode from settings.json's `permissions.defaultMode`.
/// Falls back to `PermissionMode::Default` when the field is absent or unrecognised.
fn initial_permission_mode(config: &ResolvedConfig) -> nonoclaw_core::PermissionMode {
    config
        .settings()
        .permissions
        .as_ref()
        .and_then(|p| p.default_mode.as_deref())
        .and_then(nonoclaw_core::PermissionMode::from_kebab)
        .unwrap_or_default()
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

fn authenticated_public_url(public_url: &str, auth_token: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(public_url).ok()?;
    let retained: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "token")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    {
        let mut query = url.query_pairs_mut();
        query.clear();
        query.extend_pairs(retained.iter().map(|(key, value)| (key, value)));
        query.append_pair("token", auth_token);
    }
    Some(url.into())
}

fn listener_requires_auth(addr: &str, tunnel: bool, public_url: Option<&str>) -> bool {
    tunnel
        || public_url.is_some()
        || addr
            .parse::<std::net::SocketAddr>()
            .map(|address| !address.ip().is_loopback())
            .unwrap_or(true)
}

async fn list_sessions_wire_for(service: SessionService, cwd: PathBuf) -> Vec<SessionInfoWire> {
    let sessions = match tokio::task::spawn_blocking(move || service.list_sessions(&cwd)).await {
        Ok(sessions) => sessions.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    sessions
        .into_iter()
        .map(|s| SessionInfoWire {
            id: s.id,
            started: s.started,
            message_count: s.message_count,
            summary: s.summary,
            title: s.title,
            tag: s.tag,
        })
        .collect()
}

// Project, media, and static responsibilities are delegated to their services.

// ── Model health probe ──────────────────────────────────────────────────────

/// Latency budget per model probe. Enough for cold starts on free gateways,
/// short enough that "run all" finishes promptly.
const MODEL_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Output budget per probe. Several provider families reject small values:
/// DeepSeek's Anthropic endpoint requires `max_tokens >= 16`, and OpenAI
/// Responses-API reasoning models (zen `gpt-5.x`) require
/// `max_output_tokens >= 256` (`integer_below_min_value` below that). 256
/// is the smallest value every format accepts; actual probe output is a few
/// tokens, so the cost difference is negligible.
const MODEL_PROBE_MAX_TOKENS: u32 = 256;

/// Fire one minimal liveness request at a single model profile.
/// Returns (ok, latency_ms on success, short error on failure).
async fn probe_model_health(config: ClientConfig) -> (bool, Option<u64>, Option<String>) {
    let model = config.model.clone();
    let started = std::time::Instant::now();
    let client = match nonoclaw_api::Client::new(config.api_key, config.auth_token, config.base_url)
    {
        Ok(client) => client.with_format(config.api_format),
        Err(err) => return (false, None, Some(err.to_string())),
    };
    // One direct request: no retry loop, no SSE parse — the real status code
    // (401/402/404/429/…) and provider message survive for display.
    // Exception: transient 5xx/429 gateways (zen's free tier flips between
    // 200 and 503 within seconds) get exactly one quick retry so the dot
    // reflects steady-state availability, not a single unlucky packet.
    let result = tokio::time::timeout(
        MODEL_PROBE_TIMEOUT,
        client.probe_liveness(&model, MODEL_PROBE_MAX_TOKENS),
    )
    .await;
    let result = match result {
        Ok(Ok((status, snippet)))
            if nonoclaw_core::Error::classify_status(status)
                == nonoclaw_core::ApiErrorKind::Retryable =>
        {
            tokio::time::timeout(
                MODEL_PROBE_TIMEOUT,
                client.probe_liveness(&model, MODEL_PROBE_MAX_TOKENS),
            )
            .await
            .or(Ok(Ok((status, snippet))))
        }
        other => other,
    };
    match result {
        Ok(Ok((status, _snippet))) if (200..300).contains(&status) => {
            (true, Some(started.elapsed().as_millis() as u64), None)
        }
        Ok(Ok((status, snippet))) => {
            let error = if snippet.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {snippet}")
            };
            (false, None, Some(short_error(&error)))
        }
        Ok(Err(err)) => (false, None, Some(short_error(&err.to_string()))),
        Err(_) => (
            false,
            None,
            Some(format!("timeout ({}s)", MODEL_PROBE_TIMEOUT.as_secs())),
        ),
    }
}

fn short_error(err: &str) -> String {
    let first = err.lines().next().unwrap_or("").trim();
    if first.chars().count() > 160 {
        format!("{}…", first.chars().take(160).collect::<String>())
    } else {
        first.to_string()
    }
}

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
    let local_ws_ticket = Uuid::new_v4().to_string().replace('-', "");
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

    let upload_dir = upload_dir_for(&cwd);
    if let Err(error) = std::fs::create_dir_all(&upload_dir) {
        tracing::warn!(kind = ?error.kind(), "cannot create upload directory");
    }

    let default_permission_mode = initial_permission_mode(&config);
    let skills_manager = Arc::new(RwLock::new(SkillsManager::new(&cwd)));
    let projects = ProjectContextStore::new(ProjectContext::new(
        1,
        cwd.clone(),
        config,
        skills_manager,
        upload_dir,
    ));
    let project_service = Arc::new(ProjectService::new(
        Arc::clone(&registry),
        public_url.clone(),
    ));
    let state = Arc::new(AppState {
        active_model: Arc::new(Mutex::new(active_model)),
        registry,
        todos,
        projects,
        auth_token,
        local_ws_ticket,
        require_auth,
        public_url,
        session_service: SessionService::new(),
        session_hub: SessionHub::new(),
        pending_permissions: Arc::new(Mutex::new(HashMap::new())),
        pending_questions: Arc::new(Mutex::new(HashMap::new())),
        permission_meta: Arc::new(Mutex::new(HashMap::new())),
        question_meta: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: Arc::new(Mutex::new(default_permission_mode)),
        project_service,
        background_registry: Arc::new(std::sync::Mutex::new(
            nonoclaw_tools::BackgroundTaskRegistry::new(),
        )),
        markitdown_path: Arc::new(Mutex::new(None)),
        last_activity: Arc::new(Mutex::new(std::time::SystemTime::now())),
        video_queue: Arc::new(super::video_service::VideoStore::new()),
    });

    // Load persisted pending permissions (survives server restarts).
    state.load_pending_permissions().await;

    // AutoDream: consolidate memory while the user is idle.
    super::dream::spawn_dream_scheduler(Arc::clone(&state), Arc::clone(&state.last_activity));

    // Spawn one watcher that follows atomically published project contexts.
    crate::skill_watcher::spawn_project_skill_watcher(state.projects.clone());

    // Build vector indexes in the background so the first Memory search of a
    // session is already warm: facts index + full-transcript session index.
    {
        let cwd = cwd.clone();
        tokio::task::spawn_blocking(move || {
            let facts = nonoclaw_tools::memory::load_facts(&cwd);
            nonoclaw_tools::memory::load_or_build_vector_index(&cwd, &facts);
            let root = nonoclaw_engine::session::home_root();
            let sessions_dir = root.map(|r| {
                r.join("projects")
                    .join(
                        cwd.to_string_lossy()
                            .trim_start_matches('/')
                            .replace('/', "-"),
                    )
                    .join("sessions")
            });
            if let Some(dir) = sessions_dir {
                if dir.is_dir() {
                    let index = nonoclaw_tools::session_index::build_index(&cwd, &dir);
                    tracing::info!(
                        sessions = index.stamps.len(),
                        chunks = index.chunks.len(),
                        "session vector index ready"
                    );
                }
            }
        });
    }

    // This explicit operator-facing startup message is the only terminal output
    // that includes the Web credential. Keep tracing and ProjectInfo limited to
    // the token-free public origin so routine logs and browser metadata do not
    // duplicate the secret.
    if let Some(ref url) = state.public_url {
        if let Some(authenticated_url) = authenticated_public_url(url, &state.auth_token) {
            eprintln!("\n  Tunnel ready: \x1b[1;33m{authenticated_url}\x1b[0m\n");
        } else {
            tracing::warn!("public URL is invalid; authenticated startup URL omitted");
        }
    }

    // Always register the WebSocket route + PWA manifest + service worker.
    let app = Router::new()
        .route(
            "/api/auth/bootstrap",
            axum::routing::post(local_auth_bootstrap),
        )
        .route("/ws", get(ws_handler))
        .route(
            "/api/download",
            axum::routing::post(super::download_service::download_handler),
        )
        .route(
            "/api/upload",
            axum::routing::post(super::upload_service::upload_handler)
                .layer(DefaultBodyLimit::max(attachments::MAX_FILE_SIZE as usize)),
        )
        .route(
            "/api/stt",
            axum::routing::post(super::speech_service::stt_handler),
        )
        .route(
            "/api/video/models",
            axum::routing::get(super::video_service::models_handler),
        )
        .route(
            "/api/video/tasks",
            axum::routing::post(super::video_service::create_handler)
                .layer(DefaultBodyLimit::max(60 * 1024 * 1024)),
        )
        .route(
            "/api/video/tasks",
            axum::routing::get(super::video_service::list_handler),
        )
        .route(
            "/api/video/tasks/:task_id",
            axum::routing::get(super::video_service::get_handler),
        )
        .route(
            "/api/video/tasks/:task_id",
            axum::routing::delete(super::video_service::delete_handler),
        )
        .route(
            "/api/video/tasks/:task_id/file",
            axum::routing::get(super::video_service::file_handler),
        )
        .route(
            "/api/sessions/:session_id/permissions",
            axum::routing::get(super::permission_api::list_pending_permissions),
        )
        .route(
            "/api/sessions/:session_id/permissions/:request_id",
            axum::routing::post(super::permission_api::resolve_permission),
        )
        .route("/api/run", axum::routing::post(super::run_api::run_handler))
        .route(
            "/api/sessions/:session_id/cancel",
            axum::routing::post(super::run_api::cancel_handler),
        )
        .route(
            "/api/sessions/:session_id/fork",
            axum::routing::post(super::fork_api::fork_session),
        )
        .route(
            "/api/sessions/:session_id/questions",
            axum::routing::get(super::permission_api::list_pending_questions),
        )
        .route(
            "/api/sessions/:session_id/questions/:request_id",
            axum::routing::post(super::permission_api::resolve_question),
        )
        .route(
            "/api/logs/raw",
            axum::routing::get(super::api_log_service::list_raw_logs),
        )
        .route(
            "/api/logs/raw/:file",
            axum::routing::get(super::api_log_service::get_raw_log),
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

async fn local_auth_bootstrap(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    let bootstrap_header_ok = headers
        .get(LOCAL_BOOTSTRAP_HEADER)
        .and_then(|value| value.to_str().ok())
        == Some("1");
    if !bootstrap_header_ok || !local_browser_request_allowed(peer.ip(), &headers, true) {
        return super::http_error::error_response(
            StatusCode::UNAUTHORIZED,
            AppError::new(
                ErrorCode::Authentication,
                "local browser bootstrap denied",
                false,
                "local_auth_bootstrap",
            )
            .with_trace_id(Uuid::new_v4().to_string()),
        );
    }

    let cookie = format!(
        "{LOCAL_AUTH_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400",
        state.local_ws_ticket
    );
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie).expect("cookie is valid"),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::PRAGMA, "no-cache")
        .body(Body::empty())
        .expect("bootstrap response is valid")
}

async fn ws_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let Some(auth_kind) =
        state.websocket_authorization(params.get("token").map(String::as_str), peer.ip(), &headers)
    else {
        return super::http_error::error_response(
            StatusCode::UNAUTHORIZED,
            AppError::new(
                ErrorCode::Authentication,
                "invalid WebSocket credential or origin",
                false,
                "websocket_authentication",
            )
            .with_trace_id(Uuid::new_v4().to_string()),
        );
    };
    let session_id = params.get("session").cloned();
    let expose_remote_token = auth_kind == WebSocketAuthKind::LocalTicket;
    ws.on_upgrade(move |socket| handle_ws(socket, state, session_id, expose_remote_token))
}

async fn send_ws_project_msg(
    state: &AppState,
    tx: &super::protocol::Tx,
    expected_generation: u64,
    msg: ServerMsg,
) -> bool {
    let Ok(text) = serde_json::to_string(&msg) else {
        return false;
    };
    // Validate only after this frame reaches the actual serialized writer.
    // Waiting behind another socket send can span a project switch; checking
    // before the Tx mutex would allow the now-stale frame through afterward.
    let mut writer = tx.lock().await;
    if state.project().generation() != expected_generation {
        return false;
    }
    if writer.send(WsMessage::Text(text)).await.is_err() {
        tracing::warn!("websocket send failed");
        return false;
    }
    true
}

async fn ensure_ws_project_generation(
    state: &AppState,
    tx: &super::protocol::Tx,
    expected_generation: u64,
) -> bool {
    if state.project().generation() == expected_generation {
        return true;
    }
    send_msg(
        tx,
        safe_error(
            ErrorCode::InvalidRequest,
            "the active project changed; reconnecting is required",
            true,
            "project_context_changed",
        ),
    )
    .await;
    false
}

async fn handle_ws(
    ws: WebSocket,
    state: Arc<AppState>,
    session_id: Option<String>,
    expose_remote_token: bool,
) {
    let (tx, mut rx) = super::protocol::split_socket(ws);
    let active_controller: Arc<Mutex<Option<RunController>>> = Arc::new(Mutex::new(None));
    // JoinHandle of the current controller supervisor adapter. Cancellation is
    // cooperative through RunController; awaiting this handle guarantees the
    // exactly-once terminal is sent before Clear or a replacement run.
    let mut run_handle: Option<tokio::task::JoinHandle<()>> = None;
    let (initial_project, mut project_updates) = state.projects.snapshot_and_subscribe();
    let mut project_generation = initial_project.generation();
    // Pair the initial context with the transition gate before touching
    // project-scoped session storage. A concurrent switch therefore happens
    // wholly before this handshake (which is rejected) or after setup.
    let initial_transition = state.projects.lock_transition().await;
    if state.project().generation() != project_generation {
        drop(initial_transition);
        let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
        return;
    }

    // Capture before any inner shadow (the Run arm destructures session_id).
    // Desktop (no URL param): auto-resume the most recent session.
    // Mobile (QR code): use the session id encoded in the QR.
    let mut shared_sid = session_id.clone().or_else(|| {
        state
            .session_service
            .most_recent_session(initial_project.cwd())
            .ok()
            .flatten()
    });

    // Register this peer without holding the hub lock across session disk I/O.
    if let Some(ref sid) = shared_sid {
        state
            .session_hub
            .register_existing(&state.session_service, initial_project.cwd(), sid, &tx)
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
        let initial_sessions = list_sessions_wire_for(
            state.session_service.clone(),
            initial_project.cwd().to_path_buf(),
        )
        .await;

        // If sharing an existing session, replay its messages so the connecting
        // peer (e.g. mobile) sees the same conversation. Otherwise, fresh.
        let existing = session.lock().await.clone();
        let Some(handle) = existing.or_else(|| {
            create_new_session(
                &state.session_service,
                initial_project.cwd(),
                initial_project.config(),
            )
        }) else {
            drop(initial_transition);
            if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                return;
            }
            let _ = send_ws_project_msg(
                &state,
                &tx,
                project_generation,
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
                drop(initial_transition);
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    return;
                }
                let _ = send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
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
        let initial_model = state.active_model.lock().await.clone();
        let cumulative_usage = state.session_hub.cumulative_usage_json(&sid).await;
        drop(initial_transition);

        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
            return;
        }
        if !send_ws_project_msg(
            &state,
            &tx,
            project_generation,
            ServerMsg::SessionList {
                sessions: initial_sessions,
            },
        )
        .await
        {
            return;
        }
        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
            return;
        }
        if !send_ws_project_msg(
            &state,
            &tx,
            project_generation,
            messages_loaded(&sid, snapshot, cumulative_usage),
        )
        .await
        {
            return;
        }
        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
            return;
        }
        if !send_ws_project_msg(
            &state,
            &tx,
            project_generation,
            ServerMsg::Info {
                model: initial_model.clone(),
                auth_token: expose_remote_token.then(|| state.auth_token.clone()),
                available_models: initial_project
                    .config()
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
        .await
        {
            return;
        }

        // Send the project file tree so the frontend can render the left rail.
        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
            return;
        }
        let file_tree = state.project_service.file_tree_for(&initial_project);
        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
            return;
        }
        if !send_ws_project_msg(
            &state,
            &tx,
            project_generation,
            ServerMsg::FileTree {
                root: nonoclaw_core::display_path(initial_project.cwd()),
                entries: file_tree,
            },
        )
        .await
        {
            return;
        }

        // Send the full project context for the Insight rail + Git pane.
        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
            return;
        }
        let info = state
            .project_service
            .snapshot_for(Arc::clone(&initial_project), &initial_model)
            .await;
        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
            return;
        }
        if !send_ws_project_msg(
            &state,
            &tx,
            project_generation,
            ServerMsg::ProjectInfo { info },
        )
        .await
        {
            return;
        }
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

    loop {
        let msg = tokio::select! {
            changed = project_updates.changed() => {
                if changed.is_err() {
                    break;
                }
                let generation = *project_updates.borrow_and_update();
                if generation == project_generation {
                    continue;
                }
                send_msg(
                    &tx,
                    safe_error(
                        ErrorCode::InvalidRequest,
                        "the active project changed; reconnecting is required",
                        true,
                        "project_context_changed",
                    ),
                )
                .await;
                break;
            }
            message = rx.next() => {
                let Some(Ok(message)) = message else {
                    break;
                };
                message
            }
        };
        let text = match &msg {
            WsMessage::Text(t) => t.clone(),
            WsMessage::Close(_) => break,
            _ => continue,
        };
        // Any inbound client message counts as user activity for the
        // AutoDream idle watcher (cheapest signal — one Mutex write).
        *state.last_activity.lock().await = std::time::SystemTime::now();

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

        // `select!` may choose a simultaneously ready client frame before the
        // watch notification. Fence every frame against the published context
        // so a stale connection cannot mutate project-global state.
        if state.project().generation() != project_generation {
            send_msg(
                &tx,
                safe_error(
                    ErrorCode::InvalidRequest,
                    "the active project changed; reconnecting is required",
                    true,
                    "project_context_changed",
                ),
            )
            .await;
            break;
        }

        match parsed {
            // ── New / Resume session ────────────────────────────────────────
            ClientMsg::NewSession => {
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let Some(handle) =
                    create_new_session(&state.session_service, project.cwd(), project.config())
                else {
                    drop(project_transition);
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        safe_error(
                            ErrorCode::Storage,
                            "session storage is unavailable",
                            true,
                            "session_create",
                        ),
                    )
                    .await;
                    continue;
                };
                let snapshot = match handle.session.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
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
                let sid = handle.session.id().to_string();
                state
                    .session_hub
                    .move_registration(shared_sid.as_deref(), &handle, &tx)
                    .await;
                shared_sid = Some(sid.clone());
                *session.lock().await = Some(handle);
                let loaded = messages_loaded(
                    &sid,
                    snapshot,
                    state.session_hub.cumulative_usage_json(&sid).await,
                );
                let info = ServerMsg::Info {
                    model: state.active_model.lock().await.clone(),
                    auth_token: expose_remote_token.then(|| state.auth_token.clone()),
                    available_models: project
                        .config()
                        .conversation_models()
                        .iter()
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
                };
                let sessions = list_sessions_wire_for(
                    state.session_service.clone(),
                    project.cwd().to_path_buf(),
                )
                .await;
                drop(project_transition);

                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(&state, &tx, project_generation, loaded).await {
                    continue;
                }
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(&state, &tx, project_generation, info).await {
                    continue;
                }
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::SessionList { sessions },
                )
                .await
                {
                    continue;
                }
            }
            ClientMsg::ResumeSession { id } => {
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let handle = match resume_session(&state.session_service, project.cwd(), &id) {
                    Ok(handle) => handle,
                    Err(_) => {
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
                            safe_error(
                                ErrorCode::NotFound,
                                "session could not be resumed",
                                false,
                                "resume_session",
                            ),
                        )
                        .await;
                        continue;
                    }
                };
                let snapshot = match handle.session.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
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
                let sid = handle.session.id().to_string();
                state
                    .session_hub
                    .move_registration(shared_sid.as_deref(), &handle, &tx)
                    .await;
                shared_sid = Some(sid.clone());
                *session.lock().await = Some(handle);
                let loaded = messages_loaded(
                    &sid,
                    snapshot,
                    state.session_hub.cumulative_usage_json(&sid).await,
                );
                let info = ServerMsg::Info {
                    model: state.active_model.lock().await.clone(),
                    auth_token: expose_remote_token.then(|| state.auth_token.clone()),
                    available_models: project
                        .config()
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
                };
                let sessions = list_sessions_wire_for(
                    state.session_service.clone(),
                    project.cwd().to_path_buf(),
                )
                .await;
                drop(project_transition);

                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(&state, &tx, project_generation, loaded).await {
                    continue;
                }
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(&state, &tx, project_generation, info).await {
                    continue;
                }
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::SessionList { sessions },
                )
                .await
                {
                    continue;
                }
            }

            // ── File tree + open-file (frontend left rail) ──────────────────
            ClientMsg::FileTree => {
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let root = nonoclaw_core::display_path(project.cwd());
                let entries = state.project_service.file_tree_for(&project);
                drop(project_transition);
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::FileTree { root, entries },
                )
                .await
                {
                    continue;
                }
            }
            ClientMsg::ProjectInfoRefresh => {
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let current_model = state.active_model.lock().await.clone();
                // The refresh may probe external runtimes, so retain immutable
                // inputs but release the transition gate first.
                drop(project_transition);
                let (updates_tx, mut updates_rx) =
                    tokio::sync::mpsc::unbounded_channel::<nonoclaw_engine::RuntimeProbeReport>();
                let updates_socket = Arc::clone(&tx);
                let updates_state = Arc::clone(&state);
                let expected_generation = project_generation;
                let md_flag = Arc::clone(&state.markitdown_path);
                let forward_updates = tokio::spawn(async move {
                    while let Some(system) = updates_rx.recv().await {
                        if updates_state.project().generation() != expected_generation {
                            break;
                        }
                        // Keep the markitdown path in sync so the upload
                        // handler can route documents through MarkItDown
                        // without waiting for the full probe to finish.
                        if system.markitdown.status == "available" {
                            *md_flag.lock().await = system.markitdown.path.clone();
                        }
                        if !send_ws_project_msg(
                            &updates_state,
                            &updates_socket,
                            expected_generation,
                            ServerMsg::SystemProbe { system },
                        )
                        .await
                        {
                            break;
                        }
                    }
                });
                let info = state
                    .project_service
                    .refresh_for(Arc::clone(&project), &current_model, move |system| {
                        let _ = updates_tx.send(system);
                    })
                    .await;
                let _ = forward_updates.await;
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::ProjectInfo { info },
                )
                .await
                {
                    continue;
                }
            }
            ClientMsg::ModelsHealthCheck => {
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let config = project.config();
                let configs: Vec<ClientConfig> = config
                    .all_models()
                    .iter()
                    .filter(|p| p.is_conversation_model())
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|name| config.client_config(Some(&name)))
                    .collect();
                drop(project_transition);
                tracing::info!(count = configs.len(), "models health check started");
                let started = std::time::Instant::now();
                let mut tasks = tokio::task::JoinSet::new();
                for cfg in configs {
                    tasks.spawn(async move {
                        let name = cfg.model.clone();
                        let (ok, latency_ms, error) = probe_model_health(cfg).await;
                        super::protocol::ModelHealthEntry {
                            name,
                            ok,
                            latency_ms,
                            error,
                        }
                    });
                }
                let mut results = Vec::new();
                while let Some(entry) = tasks.join_next().await {
                    if let Ok(entry) = entry {
                        results.push(entry);
                    }
                }
                results.sort_by(|a, b| a.name.cmp(&b.name));
                tracing::info!(
                    total = results.len(),
                    ok = results.iter().filter(|r| r.ok).count(),
                    elapsed_s = started.elapsed().as_secs(),
                    "models health check finished"
                );
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::ModelsHealth { results },
                )
                .await
                {
                    continue;
                }
            }
            ClientMsg::GitShow { sha } => {
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                drop(project_transition);
                let output = state.project_service.git_show_for(&project, &sha).await;
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                let response = match output {
                    Some(output) => ServerMsg::GitShow { sha, output },
                    None => safe_error(
                        ErrorCode::NotFound,
                        "commit is invalid or unavailable",
                        false,
                        "git_show",
                    ),
                };
                if !send_ws_project_msg(&state, &tx, project_generation, response).await {
                    continue;
                }
            }
            ClientMsg::SessionPrompts { session_id } => {
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                const PROMPT_PREVIEW_CHARS: usize = 40;
                let prompts = super::protocol::session_run_prompts(
                    project.cwd(),
                    &session_id,
                    PROMPT_PREVIEW_CHARS,
                );
                drop(project_transition);
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::SessionPrompts {
                        session_id,
                        prompts,
                    },
                )
                .await
                {
                    continue;
                }
            }
            ClientMsg::LoadOlder {
                session_id,
                limit,
                before,
            } => {
                // Serve one older page for the session this connection is
                // viewing. `before` defaults to the count the client holds
                // (older messages exist behind that boundary only when the
                // restore payload was a tail window).
                let selected_session = session
                    .lock()
                    .await
                    .as_ref()
                    .map(|handle| handle.session.clone());
                let Some(selected_session) = selected_session else {
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        safe_error(
                            ErrorCode::NotFound,
                            "no active session",
                            false,
                            "load_older",
                        ),
                    )
                    .await;
                    continue;
                };
                if selected_session.id() != session_id {
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        safe_error(
                            ErrorCode::NotFound,
                            "session is not active",
                            false,
                            "load_older",
                        ),
                    )
                    .await;
                    continue;
                }
                let snapshot = match selected_session.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
                            safe_error(
                                ErrorCode::Storage,
                                "history page unavailable",
                                true,
                                "load_older",
                            ),
                        )
                        .await;
                        continue;
                    }
                };
                let total = snapshot.messages.len();
                let before = before.unwrap_or(0).min(total);
                let page = match selected_session
                    .history_page(before, limit.max(1).min(500))
                    .await
                {
                    Ok(page) => page,
                    Err(_) => {
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
                            safe_error(
                                ErrorCode::Storage,
                                "history page unavailable",
                                true,
                                "load_older",
                            ),
                        )
                        .await;
                        continue;
                    }
                };
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    super::protocol::history_page(
                        &session_id,
                        page.messages,
                        before,
                        total,
                        snapshot.revision,
                        snapshot.started,
                    ),
                )
                .await
                {
                    continue;
                }
            }
            ClientMsg::OpenFile { path, force_code } => {
                // Opening may create the requested file, so keep replacement
                // fenced through path confinement and the side effect.
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let result = state.project_service.open_for(&project, &path, force_code);
                drop(project_transition);
                if let Err(error) = result {
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    let kind = error.kind();
                    let message = match kind {
                        std::io::ErrorKind::PermissionDenied => {
                            "file outside project or home directory"
                        }
                        std::io::ErrorKind::NotFound => "file or parent directory not found",
                        _ => "failed to open file with system editor",
                    };
                    tracing::warn!(kind = ?kind, "open-file failed (path redacted)");
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        safe_error(ErrorCode::PathDenied, message, false, "open_file"),
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
                let session_for_run = {
                    let guard = session.lock().await;
                    guard.as_ref().map(|handle| handle.session.clone())
                };
                let Some(session_for_run) = session_for_run else {
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
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

                // Preserve the WS supersede contract across every entry point:
                // cancel the session's globally owned run, not merely a
                // controller created by this connection.
                let cancel_result = state
                    .session_hub
                    .cancel_run_and_wait(&session_id, "superseded by a new run")
                    .await;
                if matches!(
                    cancel_result,
                    CancelRunResult::NotCancellable | CancelRunResult::TimedOut
                ) {
                    send_msg(
                        &tx,
                        safe_error(
                            ErrorCode::InvalidRequest,
                            if cancel_result == CancelRunResult::TimedOut {
                                "the previous run did not stop in time"
                            } else {
                                "the session is performing non-cancellable exclusive work"
                            },
                            true,
                            "run_lease",
                        ),
                    )
                    .await;
                    continue;
                }
                if let Some(controller) = active_controller.lock().await.as_ref() {
                    controller.cancel("superseded by a new run");
                }
                if let Some(handle) = run_handle.take() {
                    let _ = handle.await;
                }
                *active_controller.lock().await = None;

                // Serialize run startup with project replacement. The guard is
                // released only after the controller is visible in SessionHub.
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let mut run_lease = match state.session_hub.try_acquire_run(&session_id, None).await
                {
                    Ok(lease) => lease,
                    Err(()) => {
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
                            safe_error(
                                ErrorCode::InvalidRequest,
                                "this session already has an active run",
                                true,
                                "run_lease",
                            ),
                        )
                        .await;
                        continue;
                    }
                };
                let session_snapshot = match session_for_run.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        run_lease.finish().await;
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
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
                    let mut mgr = project.skills_manager().write().unwrap();
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
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let mut ready_tx = Some(ready_tx);
                run_handle = Some(tokio::spawn(async move {
                    // If executing in fork context: run as a fresh sub-engine.
                    if let Some(body) = fork_body {
                        let qr: Arc<dyn QuestionResolver> = Arc::new(WsQuestionResolver {
                            pending: Arc::clone(&s.pending_questions),
                            meta: Arc::clone(&s.question_meta),
                            tx: tx2.clone(),
                            session_id: session_id.clone(),
                        });
                        let fork_opts = {
                            let mut o = build_options(
                                project.config(),
                                model_used.clone(),
                                None,
                                Some(body.clone()),
                                arguments.clone(),
                                tx2.clone(),
                                Arc::clone(&s.pending_permissions),
                                *s.permission_mode.lock().await,
                                Arc::clone(project.skills_manager()),
                                Arc::clone(&s.background_registry),
                                Arc::clone(&s.permission_meta),
                                session_id.clone(),
                            );
                            o.max_turns = o.max_turns.min(20);
                            o.is_non_interactive = true;
                            o.question_resolver = Some(qr);
                            o
                        };
                        let fork_client = match project
                            .config()
                            .client_for(ClientPurpose::Subagent, Some(&model_used))
                        {
                            Ok(client) => client,
                            Err(err) => {
                                tracing::warn!(model = %model_used, error = %err, "subagent client build failed");
                                drop(project_transition);
                                if let Some(ready) = ready_tx.take() {
                                    let _ = ready.send(());
                                }
                                if ensure_ws_project_generation(&s, &tx2, project.generation())
                                    .await
                                {
                                    let _ = send_ws_project_msg(
                                        &s,
                                        &tx2,
                                        project.generation(),
                                        ServerMsg::Error {
                                            error: model_client_error(
                                                project.config(),
                                                &model_used,
                                                &err,
                                                "build_subagent_client",
                                            ),
                                        },
                                    )
                                    .await;
                                }
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
                            project.cwd().to_path_buf(),
                            model_used.clone(),
                            fork_limits,
                        ));
                        if run_lease.set_controller(controller.clone()).await.is_err() {
                            drop(project_transition);
                            if let Some(ready) = ready_tx.take() {
                                let _ = ready.send(());
                            }
                            if ensure_ws_project_generation(&s, &tx2, project.generation()).await {
                                let _ = send_ws_project_msg(
                                    &s,
                                    &tx2,
                                    project.generation(),
                                    safe_error(
                                        ErrorCode::Internal,
                                        "the reserved run slot was lost",
                                        true,
                                        "run_lease",
                                    ),
                                )
                                .await;
                            }
                            return;
                        }
                        *active_for_run.lock().await = Some(controller.clone());
                        drop(project_transition);
                        if let Some(ready) = ready_tx.take() {
                            let _ = ready.send(());
                        }
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
                        run_lease.finish().await;
                        return;
                    }

                    let mut options = build_options(
                        project.config(),
                        model_used.clone(),
                        max_turns,
                        append_system_prompt.clone(),
                        arguments.clone(),
                        tx2.clone(),
                        Arc::clone(&s.pending_permissions),
                        *s.permission_mode.lock().await,
                        Arc::clone(project.skills_manager()),
                        Arc::clone(&s.background_registry),
                        Arc::clone(&s.permission_meta),
                        session_id.clone(),
                    );

                    // Question resolver (per-run to avoid oneshot key clashes).
                    let qr: Arc<dyn QuestionResolver> = Arc::new(WsQuestionResolver {
                        pending: Arc::clone(&s.pending_questions),
                        meta: Arc::clone(&s.question_meta),
                        tx: tx2.clone(),
                        session_id: session_id.clone(),
                    });
                    options.question_resolver = Some(qr);

                    // Resolve credentials/format from the same immutable
                    // snapshot. No process environment is changed when a Web
                    // session selects a different model.
                    let run_client = match project
                        .config()
                        .client_for(ClientPurpose::Conversation, Some(&model_used))
                    {
                        Ok(client) => client,
                        Err(err) => {
                            tracing::warn!(model = %model_used, error = %err, "resolved run client build failed");
                            drop(project_transition);
                            if let Some(ready) = ready_tx.take() {
                                let _ = ready.send(());
                            }
                            if ensure_ws_project_generation(&s, &tx2, project.generation()).await {
                                let _ = send_ws_project_msg(
                                    &s,
                                    &tx2,
                                    project.generation(),
                                    ServerMsg::Error {
                                        error: model_client_error(
                                            project.config(),
                                            &model_used,
                                            &err,
                                            "build_model_client",
                                        ),
                                    },
                                )
                                .await;
                            }
                            return;
                        }
                    };

                    let include_attachment_images = run_client
                        .capabilities_for_model(&model_used)
                        .status(nonoclaw_api::ProviderFeature::Images)
                        .is_supported();
                    let session_for_wire = session_for_run.clone();
                    let attachment_max_chars = nonoclaw_engine::ContextBudget::chars(
                        options.context_budget.attachment_tokens,
                        options.chars_per_token,
                    );
                    let engine = QueryEngine::with_session(
                        run_client,
                        s.registry.clone(),
                        s.todos.clone(),
                        options,
                        session_for_run,
                        session_snapshot,
                    );

                    // Text extraction/OCR is always included. Raw image blocks
                    // are added only when the selected provider accepts them
                    // and the real encoded payload fits the attachment budget.
                    let enriched = enrich_prompt_with_attachments(
                        &prompt,
                        &attachments,
                        project.upload_dir(),
                        include_attachment_images,
                        attachment_max_chars,
                    );
                    let controller =
                        RunController::for_engine(&engine, project.cwd().to_path_buf());
                    if run_lease.set_controller(controller.clone()).await.is_err() {
                        drop(project_transition);
                        if let Some(ready) = ready_tx.take() {
                            let _ = ready.send(());
                        }
                        if ensure_ws_project_generation(&s, &tx2, project.generation()).await {
                            let _ = send_ws_project_msg(
                                &s,
                                &tx2,
                                project.generation(),
                                safe_error(
                                    ErrorCode::Internal,
                                    "the reserved run slot was lost",
                                    true,
                                    "run_lease",
                                ),
                            )
                            .await;
                        }
                        return;
                    }
                    *active_for_run.lock().await = Some(controller.clone());
                    drop(project_transition);
                    if let Some(ready) = ready_tx.take() {
                        let _ = ready.send(());
                    }

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
                    // Level-1 RL label: persist the terminal outcome + heuristic reward.
                    {
                        let (status, detail, turns) = match (&terminal.status, &terminal.reason) {
                            (RunTerminalStatus::Done, reason) => {
                                let turns = terminal.result.as_ref().map(|r| r.turns).unwrap_or(0);
                                let detail = match reason {
                                    nonoclaw_engine::RunFinishReason::Completed { detail } => {
                                        detail.clone()
                                    }
                                    other => format!("{other:?}"),
                                };
                                ("done", detail, turns)
                            }
                            (RunTerminalStatus::Cancelled, reason) => {
                                let detail = match reason {
                                    nonoclaw_engine::RunFinishReason::Cancelled { reason } => {
                                        reason.clone()
                                    }
                                    other => format!("{other:?}"),
                                };
                                ("cancelled", detail, 0)
                            }
                            (RunTerminalStatus::Error, reason) => {
                                let detail = match reason {
                                    nonoclaw_engine::RunFinishReason::Error { message, .. } => {
                                        nonoclaw_core::redact_text(message)
                                    }
                                    other => format!("{other:?}"),
                                };
                                ("error", detail, 0)
                            }
                        };
                        let (reward, detail) = nonoclaw_engine::jev_reward::run_reward_with_jev(
                            status,
                            &detail,
                            turns,
                            &nonoclaw_engine::session::RewardSignals::default(),
                        )
                        .await;
                        if let Err(e) = session_for_wire
                            .write_run_outcome(
                                &terminal.run_id,
                                status,
                                reward,
                                turns,
                                &detail,
                                &terminal.usage(),
                            )
                            .await
                        {
                            tracing::warn!(error = %e, "failed to persist run outcome");
                        }
                    }
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
                                run_lease.finish().await;
                                return;
                            };
                            tracing::info!(
                                turns = r.turns,
                                text_len = r.text.len(),
                                "engine run complete"
                            );
                            // Accumulate real API token usage for the session so
                            // the frontend can restore it after a page refresh.
                            s.session_hub.accumulate_usage(&session_id, &r.usage).await;
                            // Incrementally refresh the session vector index
                            // (fingerprints make unchanged files a no-op).
                            {
                                let cwd = project.cwd().to_path_buf();
                                tokio::task::spawn_blocking(move || {
                                    let root = nonoclaw_engine::session::home_root();
                                    let Some(dir) = root.map(|r| {
                                        r.join("projects")
                                            .join(
                                                cwd.to_string_lossy()
                                                    .trim_start_matches('/')
                                                    .replace('/', "-"),
                                            )
                                            .join("sessions")
                                    }) else {
                                        return;
                                    };
                                    if dir.is_dir() {
                                        nonoclaw_tools::session_index::build_index(&cwd, &dir);
                                    }
                                });
                            }
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
                            let info = s
                                .project_service
                                .snapshot_for(Arc::clone(&project), &current_model)
                                .await;
                            send_msg(&tx2, ServerMsg::ProjectInfo { info: info.clone() }).await;

                            // Push a refreshed session list so newly-persisted
                            // sessions appear in the picker immediately.
                            send_msg(
                                &tx2,
                                ServerMsg::SessionList {
                                    sessions: list_sessions_wire_for(
                                        s.session_service.clone(),
                                        project.cwd().to_path_buf(),
                                    )
                                    .await,
                                },
                            )
                            .await;

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
                            let (reason_text, retryable, status) = match &terminal.reason {
                                nonoclaw_engine::RunFinishReason::Error {
                                    message,
                                    retryable,
                                    status,
                                } => (nonoclaw_core::redact_text(message), *retryable, *status),
                                other => (format!("{other:?}"), false, None),
                            };
                            tracing::error!(
                                run_id = %run_id,
                                retryable,
                                ?status,
                                reason = %reason_text,
                                "engine run failed"
                            );
                            let error_message = safe_provider_failure_message(&reason_text, status);
                            let error_code = if retryable || status.is_some() {
                                ErrorCode::ProviderUnavailable
                            } else {
                                ErrorCode::Internal
                            };
                            let mut app_error =
                                AppError::new(error_code, error_message, retryable, "run");
                            if let Some(code) = status {
                                app_error = app_error
                                    .with_safe_details(serde_json::json!({ "status": code }));
                            }
                            send_msg(
                                &tx2,
                                ServerMsg::RunError {
                                    protocol_version,
                                    run_id,
                                    session_id,
                                    session_revision,
                                    sequence,
                                    timestamp_ms,
                                    error: app_error.with_trace_id(Uuid::new_v4().to_string()),
                                },
                            )
                            .await;
                        }
                    }
                    *active_for_run.lock().await = None;
                    run_lease.finish().await;
                }));
                // Do not accept a follow-up Cancel/Clear until the shared
                // controller has been registered (or setup has failed).
                let _ = ready_rx.await;
            }

            // ── Cancel ──────────────────────────────────────────────────────
            ClientMsg::Cancel => {
                if let Some(ref sid) = shared_sid {
                    let _ = state
                        .session_hub
                        .cancel_run(sid, "user requested cancellation")
                        .await;
                }
                // Keep the local handle only as a join point; SessionHub owns
                // cancellation so REST and every WebSocket peer see one run.
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
                let project_transition = state.projects.lock_transition().await;
                if state.project().generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let new_mode = match mode.as_str() {
                    "auto" => nonoclaw_core::PermissionMode::Auto,
                    "bypass" | "bypassPermissions" => {
                        nonoclaw_core::PermissionMode::BypassPermissions
                    }
                    "plan" => nonoclaw_core::PermissionMode::Plan,
                    "acceptEdits" => nonoclaw_core::PermissionMode::AcceptEdits,
                    "sandboxWorkspaceWrite" | "sandbox-workspace-write" => {
                        nonoclaw_core::PermissionMode::SandboxWorkspaceWrite
                    }
                    "sandboxReadOnly" | "sandbox-read-only" => {
                        nonoclaw_core::PermissionMode::SandboxReadOnly
                    }
                    _ => nonoclaw_core::PermissionMode::Default,
                };
                *state.permission_mode.lock().await = new_mode;
                drop(project_transition);
                tracing::info!(?new_mode, "permission mode switched");
            }

            // ── Switch active model ────────────────────────────────────
            ClientMsg::SetModel { name } => {
                // Verify and update against one project while replacement is
                // fenced, so a stale socket cannot overwrite the next model.
                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                if project.config().all_models().iter().any(|p| p.name == name) {
                    // Only session state changes. Client credentials are derived
                    // per run from ResolvedConfig, avoiding process-wide races.
                    *state.active_model.lock().await = name.clone();
                    drop(project_transition);
                    tracing::info!(%name, "active model switched");
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    // Push updated Info + ProjectInfo so the UI reflects the new model immediately.
                    let info_message = ServerMsg::Info {
                        model: name.clone(),
                        auth_token: expose_remote_token.then(|| state.auth_token.clone()),
                        available_models: project
                            .config()
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
                    };
                    if !send_ws_project_msg(&state, &tx, project_generation, info_message).await {
                        continue;
                    }
                    let info = state
                        .project_service
                        .snapshot_for(Arc::clone(&project), &name)
                        .await;
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    if !send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        ServerMsg::ProjectInfo { info },
                    )
                    .await
                    {
                        continue;
                    }
                } else {
                    drop(project_transition);
                    tracing::warn!("unknown model requested — ignored");
                }
            }

            // ── Switch project working directory ──────────────────────
            ClientMsg::SwitchProject { path } => {
                // Run startup and project replacement share this gate. Once it
                // is held, a global active-run check cannot race with a new
                // WS, REST, or AutoDream run acquiring its session lease.
                let project_transition = state.projects.lock_transition().await;
                if state.project().generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                if state.session_hub.has_active_runs().await {
                    drop(project_transition);
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        safe_error(
                            ErrorCode::InvalidRequest,
                            "cannot switch project while a run is active",
                            false,
                            "switch_project",
                        ),
                    )
                    .await;
                    continue;
                }

                let current_project = state.project();
                // Accept both slash styles without byte-slicing arbitrary
                // UTF-8 input. Relative paths resolve against one cwd snapshot.
                let has_drive_prefix = path.as_bytes().get(1) == Some(&b':');
                let raw = if has_drive_prefix {
                    std::path::PathBuf::from(path.replace('/', r"\"))
                } else {
                    std::path::PathBuf::from(&path)
                };
                let joined = if raw.is_absolute() {
                    raw
                } else {
                    current_project.cwd().join(raw)
                };
                let Some(resolved) = joined.canonicalize().ok().filter(|p| p.is_dir()) else {
                    drop(project_transition);
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        safe_error(
                            ErrorCode::InvalidRequest,
                            "project path is not a directory",
                            false,
                            "switch_project",
                        ),
                    )
                    .await;
                    continue;
                };

                // Fully prepare config, skills, and upload storage before
                // changing any globally visible project pointer.
                let next = match state.projects.prepare(resolved.clone()) {
                    Ok(next) => next,
                    Err(error) => {
                        tracing::warn!(kind = ?error.kind(), "project preparation failed");
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
                            safe_error(
                                ErrorCode::Storage,
                                "the new project could not be prepared",
                                true,
                                "switch_project",
                            ),
                        )
                        .await;
                        continue;
                    }
                };
                let current_model = state.active_model.lock().await.clone();
                let selected_model = if next
                    .config()
                    .conversation_models()
                    .iter()
                    .any(|profile| profile.name == current_model)
                {
                    current_model
                } else {
                    next.config().active_model.value.clone()
                };

                // Open the replacement session before publication as well, so
                // failure leaves the old project and connection intact.
                let new_handle = state
                    .session_service
                    .most_recent_session(next.cwd())
                    .ok()
                    .flatten()
                    .and_then(|sid| resume_session(&state.session_service, next.cwd(), &sid).ok())
                    .or_else(|| {
                        create_new_session(&state.session_service, next.cwd(), next.config())
                    });
                let Some(handle) = new_handle else {
                    drop(project_transition);
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        safe_error(
                            ErrorCode::Storage,
                            "cannot open a session for the new project",
                            true,
                            "switch_project",
                        ),
                    )
                    .await;
                    continue;
                };
                let snapshot = match handle.session.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
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
                let sid = handle.session.id().to_string();

                // Persist old-project metadata, then swap every project-scoped
                // input atomically and load metadata from the new project.
                state.persist_pending_permissions().await;
                state.pending_permissions.lock().await.clear();
                state.pending_questions.lock().await.clear();
                state.permission_meta.lock().await.clear();
                state.question_meta.lock().await.clear();
                *state.active_model.lock().await = selected_model.clone();
                *state.permission_mode.lock().await = initial_permission_mode(next.config());
                project_generation = next.generation();
                state.projects.replace(Arc::clone(&next));
                state.load_pending_permissions().await;

                state
                    .session_hub
                    .move_registration(shared_sid.as_deref(), &handle, &tx)
                    .await;
                shared_sid = Some(sid.clone());
                *session.lock().await = Some(handle);
                drop(project_transition);
                tracing::info!(dir = %resolved.display(), "project switched");

                let cumulative_usage = state.session_hub.cumulative_usage_json(&sid).await;
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    messages_loaded(&sid, snapshot, cumulative_usage),
                )
                .await
                {
                    continue;
                }
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::Info {
                        model: selected_model.clone(),
                        auth_token: expose_remote_token.then(|| state.auth_token.clone()),
                        available_models: next
                            .config()
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
                .await
                {
                    continue;
                }
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                let file_tree = state.project_service.file_tree_for(&next);
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::FileTree {
                        root: nonoclaw_core::display_path(next.cwd()),
                        entries: file_tree,
                    },
                )
                .await
                {
                    continue;
                }
                let sessions =
                    list_sessions_wire_for(state.session_service.clone(), next.cwd().to_path_buf())
                        .await;
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::SessionList { sessions },
                )
                .await
                {
                    continue;
                }
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                let info = state
                    .project_service
                    .snapshot_for(Arc::clone(&next), &selected_model)
                    .await;
                if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                    continue;
                }
                if !send_ws_project_msg(
                    &state,
                    &tx,
                    project_generation,
                    ServerMsg::ProjectInfo { info },
                )
                .await
                {
                    continue;
                }
            }

            // ── Clear (in-memory only; on-disk transcript is the archive) ───
            ClientMsg::Clear => {
                let canonical = session
                    .lock()
                    .await
                    .as_ref()
                    .map(|handle| handle.session.clone());
                let Some(canonical) = canonical else {
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        safe_error(
                            ErrorCode::InvalidRequest,
                            "no session is selected",
                            false,
                            "clear_session",
                        ),
                    )
                    .await;
                    continue;
                };
                let clear_session_id = canonical.id().to_string();
                let project_transition = state.projects.lock_transition().await;
                if state.project().generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }

                // Cancel and wait while run startup is fenced, then replace the
                // old run with a non-cancellable mutation lease. No new writer
                // can enter between the wait and the atomic clear.
                let cancel_result = state
                    .session_hub
                    .cancel_run_and_wait(&clear_session_id, "session cleared")
                    .await;
                if matches!(
                    cancel_result,
                    CancelRunResult::NotCancellable | CancelRunResult::TimedOut
                ) {
                    drop(project_transition);
                    if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                        continue;
                    }
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
                        safe_error(
                            ErrorCode::InvalidRequest,
                            if cancel_result == CancelRunResult::TimedOut {
                                "the active run did not stop in time"
                            } else {
                                "the session is performing non-cancellable exclusive work"
                            },
                            true,
                            "clear_session",
                        ),
                    )
                    .await;
                    continue;
                }
                // A locally owned run may belong to the previously selected
                // session; cancel it as a defensive connection-local fallback.
                if let Some(controller) = active_controller.lock().await.as_ref() {
                    controller.cancel("session cleared");
                }
                if let Some(handle) = run_handle.take() {
                    let _ = handle.await;
                }
                *active_controller.lock().await = None;
                let clear_lease = match state
                    .session_hub
                    .try_acquire_run(&clear_session_id, None)
                    .await
                {
                    Ok(lease) => lease,
                    Err(()) => {
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
                            safe_error(
                                ErrorCode::InvalidRequest,
                                "this session already has active work",
                                true,
                                "clear_session",
                            ),
                        )
                        .await;
                        continue;
                    }
                };
                drop(project_transition);

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
                    clear_lease.finish().await;
                    continue;
                }
                let snapshot = match canonical.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
                            safe_error(
                                ErrorCode::Storage,
                                "session snapshot is unavailable",
                                true,
                                "session_snapshot",
                            ),
                        )
                        .await;
                        clear_lease.finish().await;
                        continue;
                    }
                };
                let cum_usage = state
                    .session_hub
                    .cumulative_usage_json(canonical.id())
                    .await;
                let ml = messages_loaded(canonical.id(), snapshot, cum_usage);
                send_msg(&tx, ml).await;

                // Broadcast the clear before releasing exclusivity, so a new
                // run cannot race an older MessagesLoaded snapshot to peers.
                if let Some(ref cid) = shared_sid {
                    state.session_hub.sync(cid, &tx).await;
                }
                clear_lease.finish().await;
            }

            // ── Manual /compact ─────────────────────────────────────────────
            ClientMsg::Compact => {
                let canonical = session
                    .lock()
                    .await
                    .as_ref()
                    .map(|handle| handle.session.clone());
                let Some(canonical) = canonical else {
                    let _ = send_ws_project_msg(
                        &state,
                        &tx,
                        project_generation,
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
                let compact_session_id = canonical.id().to_string();
                let cancel_result = state
                    .session_hub
                    .cancel_run_and_wait(&compact_session_id, "manual compaction requested")
                    .await;
                if matches!(
                    cancel_result,
                    CancelRunResult::NotCancellable | CancelRunResult::TimedOut
                ) {
                    send_msg(
                        &tx,
                        safe_error(
                            ErrorCode::InvalidRequest,
                            if cancel_result == CancelRunResult::TimedOut {
                                "the active run did not stop in time"
                            } else {
                                "the session is already performing non-cancellable exclusive work"
                            },
                            true,
                            "compact_session",
                        ),
                    )
                    .await;
                    continue;
                }
                if let Some(controller) = active_controller.lock().await.as_ref() {
                    controller.cancel("manual compaction requested");
                }
                if let Some(handle) = run_handle.take() {
                    let _ = handle.await;
                }
                *active_controller.lock().await = None;

                let project_transition = state.projects.lock_transition().await;
                let project = state.project();
                if project.generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                // Reserve the session before reading its transcript. The
                // snapshot and the compaction engine then describe the same
                // exclusive interval, rather than a stale pre-lease view.
                let compact_lease = match state
                    .session_hub
                    .try_acquire_run(&compact_session_id, None)
                    .await
                {
                    Ok(lease) => lease,
                    Err(()) => {
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
                            safe_error(
                                ErrorCode::InvalidRequest,
                                "this session already has active work",
                                true,
                                "compact_session",
                            ),
                        )
                        .await;
                        continue;
                    }
                };
                let snapshot = match canonical.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        compact_lease.finish().await;
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
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
                let compact_start_revision = snapshot.revision;
                let compact_for_model = state.active_model.lock().await.clone();
                let options = build_options(
                    project.config(),
                    compact_for_model.clone(),
                    None,
                    None,
                    None,
                    tx.clone(),
                    Arc::clone(&state.pending_permissions),
                    *state.permission_mode.lock().await,
                    Arc::clone(project.skills_manager()),
                    Arc::clone(&state.background_registry),
                    Arc::clone(&state.permission_meta),
                    compact_session_id.clone(),
                );
                let compact_client = match project
                    .config()
                    .client_for(ClientPurpose::Conversation, Some(&compact_for_model))
                {
                    Ok(client) => client,
                    Err(err) => {
                        tracing::warn!(model = %compact_for_model, error = %err, "compact client build failed");
                        compact_lease.finish().await;
                        drop(project_transition);
                        if !ensure_ws_project_generation(&state, &tx, project_generation).await {
                            continue;
                        }
                        let _ = send_ws_project_msg(
                            &state,
                            &tx,
                            project_generation,
                            ServerMsg::Error {
                                error: model_client_error(
                                    project.config(),
                                    &compact_for_model,
                                    &err,
                                    "build_model_client",
                                ),
                            },
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
                drop(project_transition);
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
                                    pruned_results: 0,
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
                                    pruned_results: 0,
                                },
                            ),
                        )
                        .await;
                    }
                    Err(_) => {
                        // Pair the Compacting event above with a terminal event
                        // so the UI's compacting indicator always clears, even
                        // when the summarizer run fails.
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
                                    pruned_results: 0,
                                },
                            ),
                        )
                        .await;
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
                compact_lease.finish().await;
            }

            // ── Permission / question resolution ────────────────────────────
            ClientMsg::PermissionDecision {
                request_id,
                decision,
            } => {
                let project_transition = state.projects.lock_transition().await;
                if state.project().generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let Some(session_id) = shared_sid.as_ref() else {
                    continue;
                };
                let key = (session_id.clone(), request_id);
                state.permission_meta.lock().await.remove(&key);
                let sender = state.pending_permissions.lock().await.remove(&key);
                if let Some(sender) = sender {
                    let decision = match decision.as_str() {
                        "allow" => PermissionDecision::allow(),
                        _ => PermissionDecision::deny("user denied"),
                    };
                    let _ = sender.send(decision);
                }
                state.persist_pending_permissions().await;
                drop(project_transition);
            }
            ClientMsg::QuestionAnswer { request_id, answer } => {
                let project_transition = state.projects.lock_transition().await;
                if state.project().generation() != project_generation {
                    drop(project_transition);
                    let _ = ensure_ws_project_generation(&state, &tx, project_generation).await;
                    continue;
                }
                let Some(session_id) = shared_sid.as_ref() else {
                    continue;
                };
                let key = (session_id.clone(), request_id);
                state.question_meta.lock().await.remove(&key);
                let sender = state.pending_questions.lock().await.remove(&key);
                if let Some(sender) = sender {
                    let _ = sender.send(answer);
                }
                drop(project_transition);
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
    // Note on Factor 6: pending permission metadata remains in the shared
    // `permission_meta` map after disconnection. The run itself is cancelled,
    // so resolving it via REST returns 410 Gone. Stale entries are cleaned up
    // lazily on next access. A future improvement would decouple run lifetime
    // from WebSocket lifetime to enable true cross-connection pause/resume.
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
    fn provider_failures_expose_only_safe_http_status() {
        assert_eq!(
            safe_provider_failure_message("provider request failed", Some(404)),
            "provider request failed (HTTP 404)"
        );
        assert_eq!(
            safe_provider_failure_message("authentication failed", Some(401)),
            "authentication failed"
        );
        assert_eq!(
            safe_provider_failure_message("provider request failed", None),
            "provider request failed"
        );
    }

    #[test]
    fn public_token_policy_keeps_loopback_low_friction() {
        assert!(token_is_authorized(false, "secret", None));
        assert!(token_is_authorized(true, "secret", Some("secret")));
        assert!(!token_is_authorized(true, "secret", None));
        assert!(!token_is_authorized(false, "secret", Some("wrong")));
    }

    #[test]
    fn authenticated_tunnel_url_includes_exactly_one_current_token() {
        assert_eq!(
            authenticated_public_url("https://example.trycloudflare.com", "current-token"),
            Some("https://example.trycloudflare.com/?token=current-token".into())
        );
        assert_eq!(
            authenticated_public_url(
                "https://public.example/app?mode=mobile&token=stale#share",
                "current-token"
            ),
            Some("https://public.example/app?mode=mobile&token=current-token#share".into())
        );
        assert_eq!(authenticated_public_url("not a URL", "current-token"), None);
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
    fn websocket_origin_policy_requires_same_origin_and_loopback_host_for_local_ticket() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let remote: IpAddr = "192.0.2.10".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:3000"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:3000"),
        );
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));

        assert!(local_browser_request_allowed(loopback, &headers, true));
        assert!(!local_browser_request_allowed(remote, &headers, true));

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        assert!(!local_browser_request_allowed(loopback, &headers, true));

        headers.insert(header::HOST, HeaderValue::from_static("attacker.example"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://attacker.example"),
        );
        assert!(!local_browser_request_allowed(loopback, &headers, true));

        assert!(token_is_authorized(true, "secret", Some("secret")));
        assert!(!token_is_authorized(true, "secret", None));
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
            ClientMsg::ModelsHealthCheck => "models_health_check",
            ClientMsg::GitShow { .. } => "git_show",
            ClientMsg::SessionPrompts { .. } => "session_prompts",
            ClientMsg::LoadOlder { .. } => "load_older",
            ClientMsg::SetPermissionMode { .. } => "set_permission_mode",
            ClientMsg::SetModel { .. } => "set_model",
            ClientMsg::SwitchProject { .. } => "switch_project",
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
            (r#"{"type":"models_health_check"}"#, "models_health_check"),
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
            facts: vec![],
            beads: vec![],
            docs: vec![],
            settings: vec![],
            cli_reference: vec![],
            config_reference: vec![],
            config_diagnostics: vec![],
            system: nonoclaw_engine::RuntimeProbeReport {
                fingerprint: "fixture".into(),
                completed_at_ms: 0,
                timeout_ms: 3_000,
                output_limit_bytes: 65_536,
                entries: vec![],
                python_venv: nonoclaw_engine::PythonVenvProbe {
                    status: "missing".into(),
                    python_path: None,
                    required: false,
                    suggestion: None,
                },
                markitdown: nonoclaw_engine::MarkItDownProbe {
                    status: "missing".into(),
                    path: None,
                    version: None,
                    suggestion: None,
                },
            },
            git: None,
            context_window: None,
            compact_threshold: 80_000,
            public_url: None,
            provider_balances: vec![],
            model_providers: vec![],
        };
        let system_probe = project_info.system.clone();
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
                context: None,
                urgency: "medium".into(),
                format: "multiple_choice".into(),
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
                auth_token: Some("t".into()),
                available_models: vec![],
            },
            ServerMsg::SessionList { sessions: vec![] },
            ServerMsg::MessagesLoaded {
                protocol_version: WS_PROTOCOL_VERSION,
                session_id: "s".into(),
                revision: 1,
                timestamp_ms: 1,
                messages: vec![],
                cumulative_usage: serde_json::json!({}),
                total: 0,
                traces: vec![],
            },
            ServerMsg::FileTree {
                root: "/fixture".into(),
                entries: vec![],
            },
            ServerMsg::SystemProbe {
                system: system_probe,
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
                "system_probe",
                "project_info",
                "git_show"
            ]
        );
    }
}
