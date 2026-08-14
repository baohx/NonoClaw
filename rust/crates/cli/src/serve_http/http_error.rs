//! Safe structured HTTP responses shared by upload and speech services.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use nonoclaw_core::{AppError, Error, ErrorCode};
use serde_json::json;

/// Build a safe configuration error for a failed model client build.
///
/// The message only carries bounded, non-sensitive facts: whether the model
/// has a profile, and the error category. Secrets, URLs, and raw provider
/// payloads never enter the message; the model name rides in `safe_details`.
pub(super) fn model_client_error(
    config: &nonoclaw_engine::ResolvedConfig,
    model: &str,
    error: &Error,
    operation: &str,
) -> AppError {
    let has_profile = config.all_models().iter().any(|p| p.name == model);
    let reason = match error {
        Error::Auth(_) => {
            if has_profile {
                "model profile has no usable API key (check apiKey / $ENV reference)"
            } else {
                "model is not defined in settings profiles and no ANTHROPIC_API_KEY fallback is set"
            }
        }
        _ => "client build failed for this model",
    };
    AppError::new(ErrorCode::Configuration, reason, false, operation)
        .with_trace_id(uuid::Uuid::new_v4().to_string())
        .with_safe_details(json!({
            "model": model,
            "has_profile": has_profile,
            "error_kind": match error {
                Error::Auth(_) => "auth",
                Error::Config(_) => "config",
                Error::Network(_) => "network",
                _ => "other",
            },
        }))
}

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

    fn test_config(models_json: serde_json::Value) -> nonoclaw_engine::ResolvedConfig {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            serde_json::to_vec(&serde_json::json!({ "models": models_json })).unwrap(),
        )
        .unwrap();
        nonoclaw_engine::load_resolved_config(temp.path(), Some(&settings_path), None)
    }

    #[test]
    fn model_client_error_identifies_unknown_model_without_profile() {
        // **Validates: safe diagnostics for Run.model that has no profile**
        let config = test_config(serde_json::json!([{
            "name": "known-model",
            "baseUrl": "https://api.example.com",
            "apiKey": "synthetic-test-key",
        }]));
        let error = Error::Auth("no ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN set".into());
        let app_error = model_client_error(&config, "glm-5.3", &error, "build_model_client");
        assert_eq!(app_error.code, ErrorCode::Configuration);
        assert!(
            app_error.message.contains("not defined in settings profiles"),
            "message should name the root cause: {}",
            app_error.message
        );
        assert!(!app_error.message.contains("ANTHROPIC_API_KEY or ANTHROPIC"), "message must stay generic");
        assert_eq!(app_error.safe_details["model"], "glm-5.3");
        assert_eq!(app_error.safe_details["has_profile"], false);
        assert_eq!(app_error.safe_details["error_kind"], "auth");
    }

    #[test]
    fn model_client_error_identifies_profile_missing_key() {
        // apiKey is a required profile field, so an empty value is the way a
        // profile can exist while still failing client build with Error::Auth.
        let config = test_config(serde_json::json!([{
            "name": "known-model",
            "baseUrl": "https://api.example.com",
            "apiKey": "",
        }]));
        let error = Error::Auth("no credentials".into());
        let app_error = model_client_error(&config, "known-model", &error, "build_model_client");
        assert!(
            app_error.message.contains("no usable API key"),
            "message should point at the profile key: {}",
            app_error.message
        );
        assert_eq!(app_error.safe_details["has_profile"], true);
    }

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
