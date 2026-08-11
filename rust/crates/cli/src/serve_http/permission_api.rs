//! REST API for listing and resolving pending permission requests (Factor 6)
//! and pending questions (Factor 7).
//!
//! These endpoints allow external systems (webhooks, CI/CD, another CLI call)
//! to approve or deny tool permissions and answer questions without an active
//! WebSocket connection. The pending request must have been surfaced via the
//! WebSocket or REST `/api/run` first — the REST API resolves the same
//! in-memory maps.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use nonoclaw_core::PermissionDecision;

use super::connection::AppState;

/// Metadata for a pending permission request, stored alongside the oneshot
/// sender so REST clients can inspect what needs approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPermissionInfo {
    pub request_id: String,
    pub tool_name: String,
    pub message: String,
    pub input: Value,
}

/// Shared store that tracks pending permission metadata.
/// The `PermissionMap` (oneshot senders) lives separately in `AppState`.
pub type PendingPermissionMeta =
    Arc<Mutex<HashMap<String, PendingPermissionInfo>>>;

/// Response body for listing pending permissions.
#[derive(Debug, Serialize)]
struct ListPermissionsResponse {
    permissions: Vec<PendingPermissionInfo>,
}

/// Request body for resolving a permission.
#[derive(Debug, Deserialize)]
pub(super) struct ResolvePermissionRequest {
    /// "allow" or "deny"
    decision: String,
    /// Optional reason (logged, shown to model on deny)
    #[serde(default)]
    reason: Option<String>,
}

/// `GET /api/sessions/:session_id/permissions` — list pending permission
/// requests for the given session.
pub async fn list_pending_permissions(
    State(state): State<Arc<AppState>>,
    Path(_session_id): Path<String>,
) -> Response {
    // We don't currently partition by session_id (permissions are global
    // per-server). In the future, session-scoped partitioning could be added.
    let metas = state.permission_meta.lock().await;
    let permissions: Vec<_> = metas.values().cloned().collect();
    Json(ListPermissionsResponse { permissions }).into_response()
}

/// `POST /api/sessions/:session_id/permissions/:request_id` — resolve a
/// pending permission request. Returns 404 if the request_id is unknown
/// or already resolved.
pub async fn resolve_permission(
    State(state): State<Arc<AppState>>,
    Path((_session_id, request_id)): Path<(String, String)>,
    Json(body): Json<ResolvePermissionRequest>,
) -> Response {
    let sender = {
        let mut pending = state.pending_permissions.lock().await;
        pending.remove(&request_id)
    };
    let Some(sender) = sender else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "permission request not found or already resolved",
                "request_id": request_id,
            })),
        )
            .into_response();
    };

    // Also clean up the metadata entry and persist to disk.
    state.permission_meta.lock().await.remove(&request_id);
    state.persist_pending_permissions().await;

    let decision = match body.decision.as_str() {
        "allow" => PermissionDecision::allow(),
        _ => PermissionDecision::deny(
            body.reason
                .unwrap_or_else(|| "denied via REST API".to_string()),
        ),
    };

    match sender.send(decision) {
        Ok(()) => Json(json!({
            "status": "resolved",
            "request_id": request_id,
            "decision": body.decision,
        }))
        .into_response(),
        Err(_) => (
            StatusCode::GONE,
            Json(json!({
                "error": "the waiting run is no longer active (process may have exited)",
                "request_id": request_id,
            })),
        )
            .into_response(),
    }
}

// ── Factor 7: Pending Questions REST API ───────────────────────────────────

/// Metadata for a pending question, stored alongside the oneshot sender so
/// REST clients can inspect what the agent is asking.
#[derive(Debug, Clone, Serialize)]
pub struct PendingQuestionInfo {
    pub request_id: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub options: Vec<String>,
    pub urgency: String,
    pub format: String,
}

/// Shared store that tracks pending question metadata.
pub type PendingQuestionMeta = Arc<Mutex<HashMap<String, PendingQuestionInfo>>>;

/// Response body for listing pending questions.
#[derive(Debug, Serialize)]
struct ListQuestionsResponse {
    questions: Vec<PendingQuestionInfo>,
}

/// Request body for answering a question.
#[derive(Debug, Deserialize)]
pub(super) struct AnswerQuestionRequest {
    /// The user's answer (must match one of `options` for multiple_choice/yes_no).
    #[serde(default)]
    answer: Option<String>,
}

/// `GET /api/sessions/:session_id/questions` — list pending questions.
pub async fn list_pending_questions(
    State(state): State<Arc<AppState>>,
    Path(_session_id): Path<String>,
) -> Response {
    let metas = state.question_meta.lock().await;
    let questions: Vec<_> = metas.values().cloned().collect();
    Json(ListQuestionsResponse { questions }).into_response()
}

/// `POST /api/sessions/:session_id/questions/:request_id` — answer a pending
/// question. Returns 404 if the request_id is unknown or already resolved.
pub async fn resolve_question(
    State(state): State<Arc<AppState>>,
    Path((_session_id, request_id)): Path<(String, String)>,
    Json(body): Json<AnswerQuestionRequest>,
) -> Response {
    let sender = {
        let mut pending = state.pending_questions.lock().await;
        pending.remove(&request_id)
    };
    let Some(sender) = sender else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "question not found or already resolved",
                "request_id": request_id,
            })),
        )
            .into_response();
    };

    // Clean up metadata.
    state.question_meta.lock().await.remove(&request_id);

    match sender.send(body.answer) {
        Ok(()) => Json(json!({
            "status": "resolved",
            "request_id": request_id,
        }))
        .into_response(),
        Err(_) => (
            StatusCode::GONE,
            Json(json!({
                "error": "the waiting run is no longer active",
                "request_id": request_id,
            })),
        )
            .into_response(),
    }
}
