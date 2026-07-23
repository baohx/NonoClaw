//! Anthropic Messages API streaming client. Mirrors `src/services/api/`.

pub mod client;
pub mod factory;
pub mod provider;
pub mod retry;
pub mod sse;

pub use client::{
    ApiFormat, Client, RequestParams, StreamEvent, SystemBlock, ThinkingConfig, ToolChoice,
    ToolSchema, TurnOutput, DEFAULT_BASE_URL,
};
pub use factory::{ClientConfig, ClientFactory, ClientPurpose};
pub use provider::{
    CapabilityStatus, ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderFeature,
    StreamFailure,
};
pub use retry::{with_retry, with_retry_notify, RetryConfig};
