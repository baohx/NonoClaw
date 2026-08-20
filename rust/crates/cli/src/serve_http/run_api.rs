//! `POST /api/run` — REST endpoint for launching an agent run without a
//! WebSocket connection (Factor 6).
//!
//! Accepts a JSON body with a prompt and optional session/model parameters.
//! Returns a streaming NDJSON response (one JSON object per line) containing
//! the same engine events as the WebSocket protocol. When the run completes,
//! a final `done` or `error` object is emitted and the stream closes.
//!
//! Permission requests and questions are auto-denied (for safety in headless
//! REST contexts). Use `permission_mode: "auto"` or `"bypassPermissions"` in
//! the request body or settings to allow autonomous operation.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use tokio::sync::mpsc;

use nonoclaw_core::{MessageContent, PermissionDecision, PermissionMode};
use nonoclaw_engine::{
    ClientPurpose, ConfigSource, QueryEngine, RunConfigOverrides, RunController, RunTerminalStatus,
};

use super::connection::AppState;
use super::session_hub::{create_new_session, resume_session};

/// Request body for `POST /api/run`.
#[derive(Debug, Deserialize)]
pub struct RunRequest {
    /// The user prompt to send to the agent.
    pub prompt: String,
    /// Resume an existing session by id. If omitted, a new session is created.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Model name override. Falls back to the server's active model.
    #[serde(default)]
    pub model: Option<String>,
    /// Maximum turns for this run.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Additional system prompt text appended for this run.
    #[serde(default)]
    pub append_system_prompt: Option<String>,
    /// Template arguments ($1, $2, ...) to substitute.
    #[serde(default)]
    pub arguments: Option<String>,
    /// Permission mode for this run. Defaults to the server's configured mode.
    /// Use "auto" or "bypassPermissions" for fully autonomous REST runs.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Internal: mark this run as a background AutoDream consolidation so the
    /// created session is tagged and skipped by auto-resume. Not part of the
    /// public REST contract (the dream scheduler sets it in-process).
    #[serde(default)]
    pub dream: bool,
}

/// Individual NDJSON line emitted by the streaming endpoint.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RunStreamItem {
    Event {
        #[serde(flatten)]
        envelope: nonoclaw_engine::EventEnvelope,
    },
    Done {
        run_id: String,
        session_id: String,
        session_revision: u64,
        text: String,
        usage: serde_json::Value,
        turns: u32,
        stop_reason: Option<String>,
    },
    Error {
        run_id: String,
        session_id: String,
        message: String,
        retryable: bool,
    },
}

pub async fn run_handler(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<RunRequest>,
) -> Response {
    // Auth check: same token-based auth as other REST endpoints.
    if !state.authorized(None) {
        return super::http_error::error_response(
            StatusCode::UNAUTHORIZED,
            nonoclaw_core::AppError::new(
                nonoclaw_core::ErrorCode::Authentication,
                "this server requires authentication — include a token query parameter",
                false,
                "rest_run_auth",
            ),
        );
    }
    run_handler_inner(state, req).await
}

/// Programmatic entry for the in-process AutoDream scheduler: same path as
/// `POST /api/run` but without axum extraction or auth (the caller is the
/// server itself). Returns the NDJSON streaming response.
pub async fn run_handler_for_dream(
    state: Arc<AppState>,
    req: RunRequest,
) -> Result<Response, String> {
    Ok(run_handler_inner(state, req).await)
}

async fn run_handler_inner(state: Arc<AppState>, req: RunRequest) -> Response {
    // External REST runs count as user activity (AutoDream idle watcher).
    *state.last_activity.lock().await = std::time::SystemTime::now();
    // Resolve or create a session.
    let session_handle = if let Some(ref id) = req.session_id {
        match resume_session(&state.session_service, &state.cwd, id) {
            Ok(handle) => handle,
            Err(e) => {
                return super::http_error::error_response(
                    StatusCode::NOT_FOUND,
                    nonoclaw_core::AppError::new(
                        nonoclaw_core::ErrorCode::NotFound,
                        format!("session could not be resumed: {e}"),
                        false,
                        "rest_run_session",
                    ),
                );
            }
        }
    } else {
        match create_new_session(&state.session_service, &state.cwd, &state.config) {
            Some(handle) => handle,
            None => {
                return super::http_error::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    nonoclaw_core::AppError::new(
                        nonoclaw_core::ErrorCode::Storage,
                        "session storage is unavailable",
                        true,
                        "rest_run_session_create",
                    ),
                );
            }
        }
    };

    let session = session_handle.session.clone();
    let session_id = session.id().to_string();
    // Tag freshly created dream sessions so auto-resume skips them.
    if req.dream && req.session_id.is_none() {
        if let Err(e) = session
            .write_tag(nonoclaw_engine::session::DREAM_SESSION_TAG)
            .await
        {
            tracing::warn!(error = %e, "failed to tag dream session");
        }
    }
    let session_snapshot = match session.snapshot().await {
        Ok(s) => s,
        Err(_) => {
            return super::http_error::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                nonoclaw_core::AppError::new(
                    nonoclaw_core::ErrorCode::Storage,
                    "session snapshot is unavailable",
                    true,
                    "rest_run_snapshot",
                ),
            );
        }
    };

    // Resolve model.
    let model_used = if let Some(m) = req.model.clone() {
        m
    } else {
        state.active_model.lock().await.clone()
    };

    // Resolve permission mode.
    let perm_mode = match req
        .permission_mode
        .as_deref()
        .and_then(PermissionMode::from_kebab)
    {
        Some(mode) => mode,
        None => *state.permission_mode.lock().await,
    };

    // Build engine options via the same canonical path as WebSocket runs.
    let mut options = state
        .config
        .resolve_run(RunConfigOverrides {
            source: ConfigSource::WebRequest {
                field: "rest_run".into(),
            },
            model: Some(model_used.clone()),
            max_turns: req.max_turns,
            permission_mode: Some(perm_mode),
            append_system_prompt: req.append_system_prompt.clone(),
            arguments: req.arguments.clone(),
            is_non_interactive: true,
            ..Default::default()
        })
        .options;

    // REST runs have no interactive resolver — auto-deny permissions and
    // auto-answer questions with "no response" if the mode requires interaction.
    // In `auto` or `bypassPermissions` mode, the permission resolver is never
    // called, so this is just a safety net.
    let pending_perms = Arc::clone(&state.pending_permissions);
    let perm_meta = Arc::clone(&state.permission_meta);
    options.permission_resolver = Some(Arc::new(move |request| {
        let pending_perms = Arc::clone(&pending_perms);
        let perm_meta = Arc::clone(&perm_meta);
        Box::pin(async move {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let request_id = uuid::Uuid::new_v4().to_string();
            pending_perms.lock().await.insert(request_id.clone(), sender);
            perm_meta.lock().await.insert(
                request_id.clone(),
                super::permission_api::PendingPermissionInfo {
                    request_id,
                    tool_name: request.tool_name,
                    message: nonoclaw_core::redact_text(&request.message),
                    input: nonoclaw_core::redact_value(request.input),
                },
            );
            receiver
                .await
                .unwrap_or_else(|_| PermissionDecision::deny("auto-denied in REST mode"))
        })
    }));
    options.is_non_interactive = true;
    options.skills_manager = Some(Arc::clone(&state.skills_manager));
    options.background_registry = Some(Arc::clone(&state.background_registry));

    // Build the client.
    let run_client = match state
        .config
        .client_for(ClientPurpose::Conversation, Some(&model_used))
    {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(model = %model_used, error = %err, "rest run client build failed");
            return super::http_error::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                super::http_error::model_client_error(
                    &state.config,
                    &model_used,
                    &err,
                    "rest_run_client",
                ),
            );
        }
    };

    let engine = QueryEngine::with_session(
        run_client,
        Arc::clone(&state.registry),
        Arc::clone(&state.todos),
        options,
        session.clone(),
        session_snapshot,
    );

    let controller = RunController::for_engine(&engine, state.cwd.clone());

    // Channel for streaming events back to the HTTP response.
    let (event_tx, event_rx) = mpsc::unbounded_channel::<RunStreamItem>();

    // Spawn the engine run in a background task.
    let session_for_run = session.clone();
    let state_for_run = Arc::clone(&state);
    tokio::spawn(async move {
        let session_for_events = session_for_run.clone();
        let event_tx_done = event_tx.clone();
        let completion = controller
            .start(engine, MessageContent::from_text(&req.prompt), move |sequenced| {
                let session_clone = session_for_events.clone();
                let tx = event_tx.clone();
                async move {
                    let revision = session_clone
                        .snapshot()
                        .await
                        .map(|s| s.revision)
                        .unwrap_or_default();
                    let envelope = sequenced.with_session_revision(revision);
                    let _ = tx.send(RunStreamItem::Event { envelope });
                }
            })
            .wait()
            .await;

        let terminal = completion.terminal;
        let revision = session_for_run
            .snapshot()
            .await
            .map(|s| s.revision)
            .unwrap_or_default();

        match terminal.status {
            RunTerminalStatus::Done => {
                if let Some(r) = terminal.result {
                    state_for_run.session_hub.accumulate_usage(&session_id, &r.usage).await;
                    let _ = event_tx_done.send(RunStreamItem::Done {
                        run_id: terminal.run_id.clone(),
                        session_id: session_id.clone(),
                        session_revision: revision,
                        text: r.text,
                        usage: serde_json::to_value(r.usage).unwrap_or_default(),
                        turns: r.turns,
                        stop_reason: r.stop_reason.as_ref().map(|s| s.as_str().to_string()),
                    });
                } else {
                    let _ = event_tx_done.send(RunStreamItem::Error {
                        run_id: terminal.run_id.clone(),
                        session_id: session_id.clone(),
                        message: "run completed without a result".into(),
                        retryable: false,
                    });
                }
            }
            RunTerminalStatus::Cancelled => {
                let _ = event_tx_done.send(RunStreamItem::Error {
                    run_id: terminal.run_id.clone(),
                    session_id: session_id.clone(),
                    message: "run cancelled".into(),
                    retryable: false,
                });
            }
            RunTerminalStatus::Error => {
                let reason = match &terminal.reason {
                    nonoclaw_engine::RunFinishReason::Error {
                        message,
                        retryable,
                        ..
                    } => (nonoclaw_core::redact_text(message), *retryable),
                    other => (format!("{other:?}"), false),
                };
                let _ = event_tx_done.send(RunStreamItem::Error {
                    run_id: terminal.run_id.clone(),
                    session_id: session_id.clone(),
                    message: reason.0,
                    retryable: reason.1,
                });
            }
        }
    });

    // Convert the mpsc receiver into an NDJSON stream using futures::stream.
    let stream = futures::stream::unfold(event_rx, |mut rx| async move {
        rx.recv().await.map(|item| {
            let line = serde_json::to_string(&item).unwrap_or_default();
            (
                Ok::<_, std::convert::Infallible>(format!("{line}\n")),
                rx,
            )
        })
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-ndjson")
        .header("cache-control", "no-store")
        .body(Body::from_stream(stream))
        .unwrap()
}
