//! Authenticated, session-scoped REST APIs for pending permissions (Factor 6)
//! and structured questions (Factor 7).
//!
//! These endpoints resolve the same oneshot channels used by WebSocket runs.
//! Every entry is keyed by `(session_id, request_id)` so one session can never
//! inspect or resolve another session's prompt.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use nonoclaw_core::{AppError, ErrorCode, PermissionDecision};

use super::connection::AppState;
use super::session_hub::valid_session_id;

pub type PendingRequestKey = (String, String);

/// Metadata for a pending permission request, stored alongside the oneshot
/// sender so authenticated REST clients can inspect what needs approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPermissionInfo {
    /// Backward-compatible default lets old persisted files be ignored rather
    /// than failing deserialization after the scope field was introduced.
    #[serde(default)]
    pub session_id: String,
    pub request_id: String,
    pub tool_name: String,
    pub message: String,
    pub input: Value,
}

pub type PendingPermissionMeta = Arc<Mutex<HashMap<PendingRequestKey, PendingPermissionInfo>>>;

#[derive(Debug, Serialize)]
struct ListPermissionsResponse {
    permissions: Vec<PendingPermissionInfo>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ResolvePermissionRequest {
    /// "allow" or "deny"
    decision: String,
    /// Optional reason (logged, shown to model on deny)
    #[serde(default)]
    reason: Option<String>,
}

fn unauthorized(operation: &'static str) -> Response {
    super::http_error::error_response(
        StatusCode::UNAUTHORIZED,
        AppError::new(
            ErrorCode::Authentication,
            "a valid local ticket or access token is required",
            false,
            operation,
        ),
    )
}

fn invalid_session_id() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "invalid session id" })),
    )
        .into_response()
}

fn request_not_found(kind: &str, request_id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": format!("{kind} request not found in this session or already resolved"),
            "request_id": request_id,
        })),
    )
        .into_response()
}

/// `GET /api/sessions/:session_id/permissions`
pub async fn list_pending_permissions(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !state.control_authorized(&headers, query.get("token").map(String::as_str)) {
        return unauthorized("list_pending_permissions");
    }
    if !valid_session_id(&session_id) {
        return invalid_session_id();
    }

    let metas = state.permission_meta.lock().await;
    let permissions = metas
        .values()
        .filter(|info| info.session_id == session_id)
        .cloned()
        .collect();
    Json(ListPermissionsResponse { permissions }).into_response()
}

/// `POST /api/sessions/:session_id/permissions/:request_id`
pub async fn resolve_permission(
    State(state): State<Arc<AppState>>,
    Path((session_id, request_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<ResolvePermissionRequest>,
) -> Response {
    if !state.control_authorized(&headers, query.get("token").map(String::as_str)) {
        return unauthorized("resolve_permission");
    }
    if !valid_session_id(&session_id) {
        return invalid_session_id();
    }

    let key = (session_id, request_id.clone());
    if state.permission_meta.lock().await.remove(&key).is_none() {
        return request_not_found("permission", &request_id);
    }
    let sender = state.pending_permissions.lock().await.remove(&key);
    state.persist_pending_permissions().await;

    let Some(sender) = sender else {
        return (
            StatusCode::GONE,
            Json(json!({
                "error": "the waiting run is no longer active",
                "request_id": request_id,
            })),
        )
            .into_response();
    };

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
                "error": "the waiting run is no longer active",
                "request_id": request_id,
            })),
        )
            .into_response(),
    }
}

// ── Factor 7: Pending Questions REST API ───────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PendingQuestionInfo {
    pub session_id: String,
    pub request_id: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub options: Vec<String>,
    pub urgency: String,
    pub format: String,
}

pub type PendingQuestionMeta = Arc<Mutex<HashMap<PendingRequestKey, PendingQuestionInfo>>>;

#[derive(Debug, Serialize)]
struct ListQuestionsResponse {
    questions: Vec<PendingQuestionInfo>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AnswerQuestionRequest {
    #[serde(default)]
    answer: Option<String>,
}

/// `GET /api/sessions/:session_id/questions`
pub async fn list_pending_questions(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !state.control_authorized(&headers, query.get("token").map(String::as_str)) {
        return unauthorized("list_pending_questions");
    }
    if !valid_session_id(&session_id) {
        return invalid_session_id();
    }

    let metas = state.question_meta.lock().await;
    let questions = metas
        .values()
        .filter(|info| info.session_id == session_id)
        .cloned()
        .collect();
    Json(ListQuestionsResponse { questions }).into_response()
}

/// `POST /api/sessions/:session_id/questions/:request_id`
pub async fn resolve_question(
    State(state): State<Arc<AppState>>,
    Path((session_id, request_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<AnswerQuestionRequest>,
) -> Response {
    if !state.control_authorized(&headers, query.get("token").map(String::as_str)) {
        return unauthorized("resolve_question");
    }
    if !valid_session_id(&session_id) {
        return invalid_session_id();
    }

    let key = (session_id, request_id.clone());
    if state.question_meta.lock().await.remove(&key).is_none() {
        return request_not_found("question", &request_id);
    }
    let sender = state.pending_questions.lock().await.remove(&key);

    let Some(sender) = sender else {
        return (
            StatusCode::GONE,
            Json(json!({
                "error": "the waiting run is no longer active",
                "request_id": request_id,
            })),
        )
            .into_response();
    };

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
