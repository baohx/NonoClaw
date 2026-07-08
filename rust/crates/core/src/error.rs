//! Error types. Mirrors the categorization in `src/services/api/errors.ts`
//! (retryable vs non-retryable, prompt-too-long, overloaded) plus tool/config
//! errors. Library crates return `nonoclaw_core::Error`; the binary converts to
//! `anyhow` for exit reporting.

use thiserror::Error;

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
