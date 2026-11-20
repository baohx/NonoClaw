//! Anthropic Messages API streaming client. Mirrors `src/services/api/`.

pub mod client;
pub mod doubao_cli;
pub mod factory;
pub mod jev;
pub mod provider;
pub mod retry;
pub mod sse;
pub mod turn;

pub use client::{
    ApiFormat, Client, RequestParams, StreamEvent, SystemBlock, ThinkingConfig, ToolChoice,
    ToolSchema, TurnOutput, DEFAULT_BASE_URL,
};
pub use doubao_cli::{DoubaoCliClient, DoubaoCliConfig};
pub use factory::{ClientConfig, ClientFactory, ClientPurpose};
pub use provider::{
    CapabilityStatus, ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderFeature,
    StreamFailure,
};
pub use retry::{with_retry, with_retry_notify, RetryConfig};
pub use turn::TurnClient;
