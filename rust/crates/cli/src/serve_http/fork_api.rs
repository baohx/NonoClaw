//! `POST /api/sessions/:session_id/fork` — branch a session at a message
//! boundary (F3 Message Fork, dsh-message-edit inspired).
//!
//! Reads the source session's JSONL, copies messages `[0..at_index]` into a
//! fresh session, and stamps a `fork:<source>#<at_index>` tag for lineage.
//! The client can then edit/resend the forked-from turn in the new branch.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::json;

use super::connection::AppState;
use super::session_hub::valid_session_id;

#[derive(Debug, Deserialize)]
pub struct ForkRequest {
    /// Copy messages [0..at_index) (exclusive). Defaults to all messages.
    #[serde(default)]
    pub at_index: Option<usize>,
    /// Optional title for the forked session.
    #[serde(default)]
    pub title: Option<String>,
}

/// `POST /api/sessions/:session_id/fork`
pub async fn fork_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    body: Option<Json<ForkRequest>>,
) -> Response {
    if !state.authorized(None) {
        return super::http_error::error_response(
            StatusCode::UNAUTHORIZED,
            nonoclaw_core::AppError::new(
                nonoclaw_core::ErrorCode::Authentication,
                "this server requires authentication — include the token in the Authorization header or query string",
                false,
                "rest_fork_auth",
            ),
        );
    }
    if !valid_session_id(&session_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid session id" })),
        )
            .into_response();
    }
    let Json(req) = body.unwrap_or(Json(ForkRequest { at_index: None, title: None }));

    let cwd = state.cwd.clone();
    let service = state.session_service.clone();

    // Load source snapshot.
    let source = match service.resume(&cwd, &session_id) {
        Ok(s) => s,
        Err(error) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("cannot open source session: {error}") })),
            )
                .into_response();
        }
    };
    let snapshot = match source.snapshot().await {
        Ok(s) => s,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("cannot read source session: {error}") })),
            )
                .into_response();
        }
    };

    let total = snapshot.messages.len();
    let at = req.at_index.unwrap_or(total).min(total);

    // Create the forked session with a fresh id.
    let new_id = nonoclaw_engine::new_session_id();
    let model = state.config.active_model.value.clone();
    let fork = match service.create(&cwd, &new_id, &model) {
        Ok(s) => s,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("cannot create fork: {error}") })),
            )
                .into_response();
        }
    };

    // Copy the prefix of messages.
    for message in snapshot.messages.into_iter().take(at) {
        if let Err(error) = fork.append(message).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed copying message: {error}") })),
            )
                .into_response();
        }
    }

    // Lineage + title metadata. Tag format doubles as a machine-readable marker.
    let _ = fork.write_tag(format!("fork:{session_id}#{at}")).await;
    if let Some(title) = req.title {
        let _ = fork.write_custom_title(title).await;
    }

    Json(json!({
        "session_id": new_id,
        "forked_from": session_id,
        "at_index": at,
        "copied_messages": at,
        "source_messages": total,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fork_copies_prefix_and_stamps_lineage() {
        use nonoclaw_core::{Message, MessageContent};
        use nonoclaw_engine::SessionService;

        let dir = tempfile::tempdir().unwrap();
        // Home-relative storage: point HOME at the tempdir, restoring the
        // original afterwards so concurrent tests can still resolve home.
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", dir.path());
        let service = SessionService::new();

        // Source session with 4 messages.
        let source = service
            .create(dir.path(), "aaaaaaaa-0000-0000-0000-000000000001", "m")
            .unwrap();
        for i in 0..4u32 {
            source
                .append(Message::user(MessageContent::from_text(format!("m{i}"))))
                .await
                .unwrap();
        }

        // Fork at index 2 (copy messages [0..2)).
        let fork = service
            .create(dir.path(), "bbbbbbbb-0000-0000-0000-000000000002", "m")
            .unwrap();
        let snap = source.snapshot().await.unwrap();
        assert_eq!(snap.messages.len(), 4);
        for message in snap.messages.into_iter().take(2) {
            fork.append(message).await.unwrap();
        }
        fork.write_tag("fork:aaaaaaaa-0000-0000-0000-000000000001#2")
            .await
            .unwrap();

        // Verify the fork sees exactly the first 2 messages + lineage tag.
        let fork_snap = fork.snapshot().await.unwrap();
        assert_eq!(fork_snap.messages.len(), 2);
        assert_eq!(fork_snap.tag.as_deref(), Some("fork:aaaaaaaa-0000-0000-0000-000000000001#2"));

        // Source is untouched.
        let source_snap = source.snapshot().await.unwrap();
        assert_eq!(source_snap.messages.len(), 4);
        match saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
}
