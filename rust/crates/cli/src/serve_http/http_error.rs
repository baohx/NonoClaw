//! Safe structured HTTP responses shared by upload and speech services.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use nonoclaw_core::AppError;

pub(super) fn error_response(status: StatusCode, error: AppError) -> Response {
    let body = serde_json::json!({
        "error": error.message,
        "code": error.code,
        "retryable": error.retryable,
        "operation": error.operation,
        "trace_id": error.trace_id,
        "safe_details": error.safe_details,
    });
    json_response(status, &body)
}

pub(super) fn json_response<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
            .header("x-content-type-options", "nosniff")
            .body(Body::from(body))
            .expect("static response is valid"),
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
            .body(Body::from(
                r#"{"error":"response serialization failed","code":"internal","retryable":false,"operation":"serialize_response","safe_details":{}}"#,
            ))
            .expect("static response is valid"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_core::ErrorCode;

    #[tokio::test]
    async fn error_response_is_structured_and_never_contains_raw_detail() {
        // **Validates: Requirements 8.8, 9.8, 11.1**
        let response = error_response(
            StatusCode::BAD_GATEWAY,
            AppError::new(
                ErrorCode::ProviderUnavailable,
                "speech service unavailable",
                true,
                "speech_to_text",
            )
            .with_safe_details(serde_json::json!({
                "authorization": "Bearer secret",
                "status": 503
            })),
        );
        let body = axum::body::to_bytes(response.into_body(), 16_384)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("provider_unavailable"));
        assert!(text.contains("speech_to_text"));
        assert!(!text.contains("Bearer secret"));
    }
}
