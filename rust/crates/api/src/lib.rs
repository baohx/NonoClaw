//! Anthropic Messages API streaming client. Mirrors `src/services/api/`.

pub mod client;
pub mod retry;
pub mod sse;

pub use client::{
    Client, RequestParams, StreamEvent, SystemBlock, ThinkingConfig, ToolChoice, ToolSchema,
    TurnOutput, DEFAULT_BASE_URL,
};
pub use retry::{with_retry, RetryConfig};
