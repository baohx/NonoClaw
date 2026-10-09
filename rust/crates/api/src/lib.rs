//! Anthropic Messages API streaming client. Mirrors `src/services/api/`.

pub mod client;
pub mod factory;
pub mod jev;
pub mod kiro;
pub mod kiro_wire;
pub mod provider;
pub mod retry;
pub mod sse;

pub use client::{
    ApiFormat, Client, RequestParams, StreamEvent, SystemBlock, ThinkingConfig, ToolChoice,
    ToolSchema, TurnOutput, DEFAULT_BASE_URL,
};
pub use factory::{ClientConfig, ClientFactory, ClientPurpose};
pub use kiro::{KiroAuthError, KiroTokenManager};
pub use kiro_wire::{
    emit_kiro_stream_events, extract_context_usage_from_frames, extract_metering_from_frames,
    extract_text_from_frames, kiro_response_message_id, kiro_response_model, kiro_response_text,
    parse_aws_event_stream, serialize_body_kiro, KiroEventFrame, KiroGenerateResponse,
    KIRO_API_URL,
};
pub use provider::{
    CapabilityStatus, ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderFeature,
    StreamFailure,
};
pub use retry::{with_retry, with_retry_notify, RetryConfig};
