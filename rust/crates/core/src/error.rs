//! Error types. Mirrors the categorization in `src/services/api/errors.ts`
//! (retryable vs non-retryable, prompt-too-long, overloaded) plus tool/config
//! errors. Library crates return `nonoclaw_core::Error`; the binary converts to
//! `anyhow` for exit reporting.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

/// Stable application-level error categories used by HTTP, WebSocket, trace,
/// and CLI adapters. Codes are intentionally less specific than internal
/// errors so credentials, paths, provider bodies, and user content cannot
/// escape through error serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Authentication,
    PayloadTooLarge,
    UnsupportedFormat,
    InvalidRequest,
    PathDenied,
    NotFound,
    Configuration,
    ProviderUnavailable,
    Storage,
    Cancelled,
    Internal,
}

/// Safe error envelope shared by every external adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub safe_details: Value,
}

fn sanitize_identifier(value: &str, max_chars: usize) -> String {
    let value = crate::redact_text(value);
    if value == "[REDACTED]" {
        return "redacted".into();
    }
    let sanitized = value
        .chars()
        .take(max_chars)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".into()
    } else {
        sanitized
    }
}

impl AppError {
    pub fn new(
        code: ErrorCode,
        message: impl Into<String>,
        retryable: bool,
        operation: impl Into<String>,
    ) -> Self {
        let message = crate::redact_text(&message.into());
        Self {
            code,
            message: if message == "[REDACTED]" {
                "operation failed".into()
            } else {
                message
            },
            retryable,
            operation: sanitize_identifier(&operation.into(), 96),
            trace_id: None,
            safe_details: json!({}),
        }
    }

    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(sanitize_identifier(&trace_id.into(), 128));
        self
    }

    /// Safe details must contain bounded technical facts only. The canonical
    /// redactor is still applied defensively before they cross a boundary.
    pub fn with_safe_details(mut self, details: Value) -> Self {
        self.safe_details = crate::redact_value(details);
        self
    }

    pub fn from_core(error: &Error, operation: impl Into<String>) -> Self {
        let operation = operation.into();
        match error {
            Error::Auth(_) => Self::new(
                ErrorCode::Authentication,
                "authentication failed",
                false,
                operation,
            ),
            Error::PromptTooLong(_) => Self::new(
                ErrorCode::InvalidRequest,
                "request exceeds the model context limit",
                false,
                operation,
            ),
            Error::Overloaded(_) => Self::new(
                ErrorCode::ProviderUnavailable,
                "provider is temporarily unavailable",
                true,
                operation,
            ),
            Error::Network(_) | Error::Timeout => Self::new(
                ErrorCode::ProviderUnavailable,
                "upstream service is temporarily unavailable",
                true,
                operation,
            ),
            Error::Cancelled => {
                Self::new(ErrorCode::Cancelled, "operation cancelled", true, operation)
            }
            Error::PermissionDenied(_, _) => Self::new(
                ErrorCode::PathDenied,
                "operation was not permitted",
                false,
                operation,
            ),
            Error::Config(_) => Self::new(
                ErrorCode::Configuration,
                "configuration is invalid or incomplete",
                false,
                operation,
            ),
            Error::Api { kind, status, .. } => Self::new(
                ErrorCode::ProviderUnavailable,
                "provider request failed",
                matches!(kind, ApiErrorKind::Retryable),
                operation,
            )
            .with_safe_details(json!({ "status": status })),
            Error::Tool { .. } | Error::Json(_) | Error::Io(_) | Error::Other(_) => Self::new(
                ErrorCode::Internal,
                "operation failed",
                error.is_retryable(),
                operation,
            ),
        }
    }
}

/// Whether an API failure is worth retrying. Mirrors the retry classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// Transient: 429, 500, 502, 503, 529 (overloaded), connection resets.
    Retryable,
    /// Permanent: 400, 401, 403, 404, prompt-too-long, content-policy, etc.
    NonRetryable,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("API error (status {status}): {message}")]
    Api {
        status: u16,
        message: String,
        kind: ApiErrorKind,
    },

    #[error("prompt too long: {0}")]
    PromptTooLong(String),

    #[error("overloaded (529): {0}")]
    Overloaded(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("authentication error: {0}")]
    Auth(String),

    #[error("request timed out")]
    Timeout,

    #[error("request cancelled")]
    Cancelled,

    #[error("tool '{tool}' failed: {message}")]
    Tool { tool: String, message: String },

    #[error("tool '{0}' denied: {1}")]
    PermissionDenied(String, String),

    #[error("config error: {0}")]
    Config(String),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Classify an HTTP status into retryable/non-retryable, matching the TS
    /// retry classifier in `src/services/api/errors.ts`.
    pub fn classify_status(status: u16) -> ApiErrorKind {
        match status {
            408 | 409 | 429 | 500 | 502 | 503 | 529 => ApiErrorKind::Retryable,
            _ => ApiErrorKind::NonRetryable,
        }
    }

    pub fn api_kind(&self) -> Option<ApiErrorKind> {
        match self {
            Error::Api { kind, .. } => Some(*kind),
            Error::Overloaded(_) => Some(ApiErrorKind::Retryable),
            Error::Network(_) | Error::Timeout => Some(ApiErrorKind::Retryable),
            _ => None,
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.api_kind() == Some(ApiErrorKind::Retryable)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_error_never_serializes_internal_secret_or_raw_error() {
        // **Validates: Requirements 8.8, 9.8, 11.1**
        let internal = Error::Network(
            "request to /home/alice/private failed with Bearer sk-proj-secret".into(),
        );
        let error = AppError::from_core(&internal, "speech_to_text")
            .with_trace_id("trace-safe")
            .with_safe_details(json!({
                "status": 503,
                "authorization": "Bearer hidden",
                "api_key": "sk-proj-hidden"
            }));
        let encoded = serde_json::to_string(&error).unwrap();
        assert!(encoded.contains("provider_unavailable"));
        assert!(encoded.contains("trace-safe"));
        assert!(!encoded.contains("alice"));
        assert!(!encoded.contains("sk-proj"));
        assert!(!encoded.contains("Bearer hidden"));
    }

    #[test]
    fn app_error_codes_and_retryability_are_stable() {
        let cases = [
            (Error::Cancelled, ErrorCode::Cancelled, true),
            (
                Error::Auth("secret".into()),
                ErrorCode::Authentication,
                false,
            ),
            (Error::Timeout, ErrorCode::ProviderUnavailable, true),
            (Error::Config("bad".into()), ErrorCode::Configuration, false),
        ];
        for (source, code, retryable) in cases {
            let error = AppError::from_core(&source, "test");
            assert_eq!(error.code, code);
            assert_eq!(error.retryable, retryable);
            assert_eq!(error.operation, "test");
        }
    }

    #[test]
    fn app_error_constructor_sanitizes_boundary_identifiers_and_unsafe_text() {
        let error = AppError::new(
            ErrorCode::Internal,
            "failed at /home/alice/private with Bearer credential",
            false,
            "run prompt/../../secret",
        )
        .with_trace_id("trace/../../Bearer secret");
        let encoded = serde_json::to_string(&error).unwrap();
        assert_eq!(error.message, "operation failed");
        assert!(!encoded.contains("alice"));
        assert!(!encoded.contains("Bearer"));
        assert!(!error.operation.contains('/'));
        assert!(!error.trace_id.as_deref().unwrap().contains('/'));
    }
}
