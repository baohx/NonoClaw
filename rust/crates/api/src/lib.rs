//! Anthropic Messages API streaming client. Mirrors `src/services/api/`.

pub mod client;
pub mod factory;
pub mod jev;
pub mod provider;
pub mod retry;
pub mod sse;

pub use client::{
    ApiFormat, Client, RequestParams, StreamEvent, SystemBlock, ThinkingConfig, ToolChoice,
    ToolSchema, TurnOutput, DEFAULT_BASE_URL, normalize_usage_part_to_anthropic_shape,
    normalize_usage_to_anthropic_shape,
};
pub use factory::{ClientConfig, ClientFactory, ClientPurpose};
pub use provider::{
    CapabilityStatus, ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderFeature,
    StreamFailure,
};
pub use retry::{with_retry, with_retry_notify, RetryConfig};
