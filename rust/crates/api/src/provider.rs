//! Provider-neutral capability and error contracts.

use std::fmt;

use nonoclaw_core::{ApiErrorKind, AppError, Error};

use crate::client::TurnOutput;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderFeature {
    Streaming,
    Thinking,
    CacheUsage,
    PromptCaching,
    Images,
    Tools,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityStatus {
    Supported,
    Unsupported { reason: &'static str },
}

impl CapabilityStatus {
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub streaming: CapabilityStatus,
    pub thinking: CapabilityStatus,
    pub cache_usage: CapabilityStatus,
    pub prompt_caching: CapabilityStatus,
    pub images: CapabilityStatus,
    pub tools: CapabilityStatus,
}

impl ProviderCapabilities {
    pub fn status(self, feature: ProviderFeature) -> CapabilityStatus {
        match feature {
            ProviderFeature::Streaming => self.streaming,
            ProviderFeature::Thinking => self.thinking,
            ProviderFeature::CacheUsage => self.cache_usage,
            ProviderFeature::PromptCaching => self.prompt_caching,
            ProviderFeature::Images => self.images,
            ProviderFeature::Tools => self.tools,
        }
    }

    pub fn require(self, feature: ProviderFeature) -> Result<(), ProviderError> {
        match self.status(feature) {
            CapabilityStatus::Supported => Ok(()),
            CapabilityStatus::Unsupported { reason } => {
                Err(ProviderError::capability(feature, reason))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorCode {
    Authentication,
    RateLimit,
    Network,
    Timeout,
    Cancelled,
    ContextLength,
    Overloaded,
    Capability,
    Stream,
    InvalidResponse,
    Api,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub code: ProviderErrorCode,
    pub status: Option<u16>,
    pub message: String,
    pub retryable: bool,
    pub operation: &'static str,
    pub feature: Option<ProviderFeature>,
}

impl ProviderError {
    pub fn capability(feature: ProviderFeature, reason: impl Into<String>) -> Self {
        Self {
            code: ProviderErrorCode::Capability,
            status: None,
            message: reason.into(),
            retryable: false,
            operation: "provider_capability",
            feature: Some(feature),
        }
    }

    pub fn cancelled() -> Self {
        Self {
            code: ProviderErrorCode::Cancelled,
            status: None,
            message: "request cancelled".into(),
            retryable: true,
            operation: "stream_turn",
            feature: None,
        }
    }

    pub fn stream(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: ProviderErrorCode::Stream,
            status: None,
            message: message.into(),
            retryable,
            operation: "read_stream",
            feature: None,
        }
    }

    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self {
            code: ProviderErrorCode::InvalidResponse,
            status: None,
            message: message.into(),
            retryable: false,
            operation: "parse_stream",
            feature: None,
        }
    }

    pub fn from_core(error: &Error, operation: &'static str) -> Self {
        let (code, status) = match error {
            Error::Api { status, kind, .. } => {
                let code = if *status == 429 {
                    ProviderErrorCode::RateLimit
                } else if matches!(kind, ApiErrorKind::Retryable) && *status >= 500 {
                    ProviderErrorCode::Overloaded
                } else {
                    ProviderErrorCode::Api
                };
                (code, Some(*status))
            }
            Error::PromptTooLong(_) => (ProviderErrorCode::ContextLength, Some(400)),
            Error::Overloaded(_) => (ProviderErrorCode::Overloaded, Some(529)),
            Error::Network(_) => (ProviderErrorCode::Network, None),
            Error::Auth(_) => (ProviderErrorCode::Authentication, None),
            Error::Timeout => (ProviderErrorCode::Timeout, None),
            Error::Cancelled => (ProviderErrorCode::Cancelled, None),
            _ => (ProviderErrorCode::Api, None),
        };
        let safe = AppError::from_core(error, operation);
        Self {
            code,
            status,
            message: safe.message,
            retryable: safe.retryable || matches!(error, Error::Cancelled),
            operation,
            feature: None,
        }
    }

    pub fn into_core(self) -> Error {
        match self.code {
            ProviderErrorCode::Authentication => Error::Auth(self.message),
            ProviderErrorCode::ContextLength => Error::PromptTooLong(self.message),
            ProviderErrorCode::Overloaded => Error::Overloaded(self.message),
            ProviderErrorCode::Network | ProviderErrorCode::Stream => Error::Network(self.message),
            ProviderErrorCode::Timeout => Error::Timeout,
            ProviderErrorCode::Cancelled => Error::Cancelled,
            ProviderErrorCode::Capability => {
                Error::Config(format!("provider capability: {}", self.message))
            }
            ProviderErrorCode::RateLimit
            | ProviderErrorCode::Api
            | ProviderErrorCode::InvalidResponse => Error::Api {
                status: self.status.unwrap_or_default(),
                message: self.message,
                kind: if self.retryable {
                    ApiErrorKind::Retryable
                } else {
                    ApiErrorKind::NonRetryable
                },
            },
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.operation, self.message)
    }
}

impl std::error::Error for ProviderError {}

#[derive(Debug, Clone)]
pub struct StreamFailure {
    pub error: ProviderError,
    /// Content, usage, model and stop information received before failure.
    pub partial: TurnOutput,
}

impl StreamFailure {
    pub fn before_stream(error: ProviderError) -> Self {
        Self {
            error,
            partial: TurnOutput::default(),
        }
    }

    pub fn into_core(self) -> Error {
        self.error.into_core()
    }
}

impl fmt::Display for StreamFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for StreamFailure {}
