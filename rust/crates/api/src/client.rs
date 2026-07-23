//! Anthropic Messages API client with streaming. Mirrors `src/services/api/`
//! (`client.ts` for auth/base-URL, `claude.ts` for the streaming consumer).
//!
//! The key faithful details (see `src/services/api/claude.ts`):
//!   * tool_use `input` arrives as a sequence of `input_json_delta.partial_json`
//!     string fragments that must be concatenated and parsed once at block end;
//!   * `usage` appears in both `message_start` and `message_delta` and is
//!     merged with the "keep the positive value" rule (`updateUsage`);
//!   * the per-turn `Usage` is accumulated into a running total across turns.

use std::collections::BTreeMap;

use futures::StreamExt;
use nonoclaw_core::{
    CacheControl, ContentBlock, Error, Message, Result, StopReason, Usage, UsagePart,
};
use serde::{Deserialize, Serialize};

use tokio_util::sync::CancellationToken;

use crate::provider::{
    CapabilityStatus, ProviderCapabilities, ProviderError, ProviderFeature, StreamFailure,
};
use crate::retry::{with_retry_notify, RetryConfig};
use crate::sse::SseParser;

const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// A system-prompt block (always text, optionally with a cache breakpoint).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    #[serde(rename = "type", default = "default_text")]
    pub kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

fn default_text() -> String {
    "text".to_string()
}

/// A tool definition sent to the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThinkingConfig {
    /// Adaptive thinking for models that support it.
    Adaptive { type_field: AdaptiveType },
    /// Explicit budget for older models.
    Enabled { budget_tokens: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveType {
    #[serde(rename = "type")]
    pub kind: String, // "adaptive"
}

impl ThinkingConfig {
    pub fn adaptive() -> Self {
        ThinkingConfig::Adaptive {
            type_field: AdaptiveType {
                kind: "adaptive".into(),
            },
        }
    }
    pub fn enabled(budget_tokens: u32) -> Self {
        ThinkingConfig::Enabled { budget_tokens }
    }
}

/// Parameters for one `messages.create` (streaming) request.
#[derive(Debug, Clone)]
pub struct RequestParams {
    pub model: String,
    pub max_tokens: u32,
    pub system: Vec<SystemBlock>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub tool_choice: Option<ToolChoice>,
    pub thinking: Option<ThinkingConfig>,
    pub temperature: Option<f64>,
    pub betas: Vec<String>,
    /// Optional safe trace label used in redacted diagnostics (for example
    /// `sess-abc:turn-3`). It never enables raw prompt persistence.
    pub trace_label: Option<String>,
}

/// Events surfaced to the caller as the stream progresses. The final folded
/// result is returned from [`Client::run_turn`]; these events enable live UI
/// (text streaming, tool-call display).
#[derive(Debug, Clone)]
pub enum StreamEvent {
    MessageStart {
        message_id: String,
        /// The model the API actually used (echoed in `message_start.message.model`).
        /// May differ from the requested alias — e.g. a configured alias resolving
        /// to `deepseek-chat` on a third-party endpoint.
        model: String,
        usage: UsagePart,
    },
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    ToolUseStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolUseInputDelta {
        index: usize,
        partial_json: String,
    },
    BlockStop {
        index: usize,
    },
    MessageDelta {
        stop_reason: Option<StopReason>,
        usage: UsagePart,
    },
    MessageStop,
    CapabilityStatus {
        feature: ProviderFeature,
        status: CapabilityStatus,
    },
    RetryScheduled {
        attempt: u32,
        delay_ms: u64,
        error: ProviderError,
    },
    StreamError {
        error: ProviderError,
        partial: TurnOutput,
    },
}

/// The fully-folded result of one streaming turn.
#[derive(Debug, Clone, Default)]
pub struct TurnOutput {
    pub message_id: String,
    /// The real model reported by the API in `message_start` (empty if absent).
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ApiFormat {
    #[default]
    Anthropic,
    OpenAI,
}

impl ApiFormat {
    pub fn capabilities(self) -> ProviderCapabilities {
        match self {
            ApiFormat::Anthropic => ProviderCapabilities {
                streaming: CapabilityStatus::Supported,
                thinking: CapabilityStatus::Supported,
                cache_usage: CapabilityStatus::Supported,
                prompt_caching: CapabilityStatus::Supported,
                images: CapabilityStatus::Supported,
                tools: CapabilityStatus::Supported,
            },
            ApiFormat::OpenAI => ProviderCapabilities {
                streaming: CapabilityStatus::Supported,
                thinking: CapabilityStatus::Unsupported {
                    reason: "OpenAI Chat Completions does not expose Anthropic thinking blocks",
                },
                cache_usage: CapabilityStatus::Unsupported {
                    reason: "OpenAI Chat Completions does not expose cache creation usage",
                },
                prompt_caching: CapabilityStatus::Unsupported {
                    reason: "OpenAI Chat Completions does not accept Anthropic cache breakpoints",
                },
                images: CapabilityStatus::Supported,
                tools: CapabilityStatus::Supported,
            },
        }
    }
}

pub struct Client {
    http: reqwest::Client,
    api_key: Option<String>,
    auth_token: Option<String>,
    base_url: String,
    retry: RetryConfig,
    format: ApiFormat,
}

impl Client {
    pub fn new(
        api_key: Option<String>,
        auth_token: Option<String>,
        base_url: String,
    ) -> Result<Self> {
        if api_key.is_none() && auth_token.is_none() {
            return Err(Error::Auth(
                "no ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN set".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("nonoclaw/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| Error::Network(e.to_string()))?;
        Ok(Client {
            http,
            api_key,
            auth_token,
            base_url,
            retry: RetryConfig::default(),
            format: ApiFormat::default(),
        })
    }

    pub fn with_format(mut self, format: ApiFormat) -> Self {
        self.format = format;
        self
    }

    pub fn api_format(&self) -> ApiFormat {
        self.format
    }

    pub fn capabilities(&self) -> ProviderCapabilities {
        self.format.capabilities()
    }

    fn validate_capabilities(
        &self,
        params: &RequestParams,
    ) -> std::result::Result<(), ProviderError> {
        let capabilities = self.capabilities();
        if params.thinking.is_some() {
            capabilities.require(ProviderFeature::Thinking)?;
        }
        Ok(())
    }

    /// Build a client from the standard environment variables:
    /// `ANTHROPIC_API_KEY` (x-api-key) and/or `ANTHROPIC_AUTH_TOKEN` (Bearer),
    /// `ANTHROPIC_BASE_URL` (default `DEFAULT_BASE_URL`).
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        let auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
        let base_url =
            std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self::new(api_key, auth_token, base_url)
    }

    /// The configured base URL (for per-run Client comparison).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    /// The configured API key, if any.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    fn build_request(&self, params: &RequestParams) -> Result<reqwest::RequestBuilder> {
        let (url, body) = match self.format {
            ApiFormat::Anthropic => {
                let body = serialize_body_anthropic(params)?;
                dump_prompt_anthropic(params, &body);
                let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
                (url, body)
            }
            ApiFormat::OpenAI => {
                let body = serialize_body_openai(params)?;
                dump_prompt_openai(params, &body);
                let url = format!(
                    "{}/v1/chat/completions",
                    self.base_url.trim_end_matches('/')
                );
                (url, body)
            }
        };
        // Write full raw context to .nonoclaw/logs/ for inspection.
        write_prompt_log(params, &body, &url);
        let mut req = self
            .http
            .post(url)
            .header("content-type", "application/json");
        match self.format {
            ApiFormat::Anthropic => {
                req = req.header("anthropic-version", ANTHROPIC_VERSION);
                if let Some(key) = &self.api_key {
                    req = req.header("x-api-key", key);
                }
                if let Some(token) = &self.auth_token {
                    req = req.header("authorization", format!("Bearer {token}"));
                }
                if !params.betas.is_empty() {
                    req = req.header("anthropic-beta", params.betas.join(","));
                }
            }
            ApiFormat::OpenAI => {
                if let Some(key) = &self.api_key {
                    req = req.header("Authorization", format!("Bearer {key}"));
                }
            }
        }
        Ok(req.body(body))
    }

    /// Build + send one streaming request, returning the live response on 2xx
    /// or a classified [`Error`] otherwise. Retried by [`Self::run_turn`].
    async fn send_request(&self, params: &RequestParams) -> Result<reqwest::Response> {
        let req = self.build_request(params)?;
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else {
            let code = status.as_u16();
            let text = resp.text().await.unwrap_or_default();
            Err(api_error_from_body(code, &text))
        }
    }

    /// Compatibility entry point. For structured partial-content failures and
    /// cancellation use [`Self::run_turn_with_cancel`].
    pub async fn run_turn(
        &self,
        params: &RequestParams,
        on_event: impl FnMut(&StreamEvent),
    ) -> Result<TurnOutput> {
        self.run_turn_with_cancel(params, on_event, CancellationToken::new())
            .await
            .map_err(StreamFailure::into_core)
    }

    /// Execute one real streaming turn. Retries are limited to failures before
    /// response headers arrive; failures after the first response preserve the
    /// partial output in both the returned [`StreamFailure`] and event stream.
    pub async fn run_turn_with_cancel(
        &self,
        params: &RequestParams,
        mut on_event: impl FnMut(&StreamEvent),
        cancel: CancellationToken,
    ) -> std::result::Result<TurnOutput, StreamFailure> {
        if request_has_cache_control(params) {
            let status = self.capabilities().status(ProviderFeature::PromptCaching);
            if !status.is_supported() {
                on_event(&StreamEvent::CapabilityStatus {
                    feature: ProviderFeature::PromptCaching,
                    status,
                });
            }
        }
        self.validate_capabilities(params)
            .map_err(StreamFailure::before_stream)?;

        let retry = self.retry.clone();
        let response = tokio::select! {
            result = with_retry_notify(
                &retry,
                || self.send_request(params),
                |attempt, delay, error| {
                    on_event(&StreamEvent::RetryScheduled {
                        attempt,
                        delay_ms: delay.as_millis().min(u128::from(u64::MAX)) as u64,
                        error: ProviderError::from_core(error, "start_stream"),
                    });
                },
            ) => result.map_err(|error| StreamFailure::before_stream(
                ProviderError::from_core(&error, "start_stream")
            ))?,
            _ = cancel.cancelled() => {
                return Err(StreamFailure::before_stream(ProviderError::cancelled()));
            }
        };

        let result = match self.format {
            ApiFormat::Anthropic => fold_anthropic_stream(response, &mut on_event, &cancel).await,
            ApiFormat::OpenAI => fold_openai_stream(response, &mut on_event, &cancel).await,
        };
        if let Err(failure) = &result {
            on_event(&StreamEvent::StreamError {
                error: failure.error.clone(),
                partial: failure.partial.clone(),
            });
        }
        result
    }
}

fn request_has_cache_control(params: &RequestParams) -> bool {
    params
        .system
        .iter()
        .any(|block| block.cache_control.is_some())
        || params.tools.iter().any(|tool| tool.cache_control.is_some())
        || params.messages.iter().any(message_has_cache_control)
}

fn message_has_cache_control(message: &Message) -> bool {
    match &message.content {
        nonoclaw_core::MessageContent::Text(_) => false,
        nonoclaw_core::MessageContent::Blocks(blocks) => blocks.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Text {
                    cache_control: Some(_),
                    ..
                }
            )
        }),
    }
}

/// Fold an Anthropic SSE response stream into a [`TurnOutput`].
async fn fold_anthropic_stream(
    response: reqwest::Response,
    on_event: &mut impl FnMut(&StreamEvent),
    cancel: &CancellationToken,
) -> std::result::Result<TurnOutput, StreamFailure> {
    let mut parser = SseParser::new();
    let mut stream = response.bytes_stream();
    let mut state = AnthropicState::default();

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(state.failure(ProviderError::cancelled()));
            }
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else { break };
        let bytes =
            chunk.map_err(|error| state.failure(ProviderError::stream(error.to_string(), true)))?;
        parser.feed_bytes(&bytes);
        while let Some(frame) = parser.next_frame() {
            if let Err(error) = handle_frame(
                &frame,
                &mut state.message_id,
                &mut state.model,
                &mut state.usage,
                &mut state.stop_reason,
                &mut state.blocks,
                on_event,
            ) {
                return Err(state.failure(ProviderError::from_core(&error, "parse_stream")));
            }
        }
    }

    state.finish()
}

#[derive(Debug, Default)]
struct AnthropicState {
    message_id: String,
    model: String,
    usage: Usage,
    stop_reason: Option<StopReason>,
    blocks: BTreeMap<usize, BlockBuilder>,
}

impl AnthropicState {
    fn partial(&self) -> TurnOutput {
        TurnOutput {
            message_id: self.message_id.clone(),
            model: self.model.clone(),
            content: self
                .blocks
                .values()
                .map(BlockBuilder::finalize_lossy)
                .collect(),
            stop_reason: self.stop_reason.clone(),
            usage: self.usage,
        }
    }

    fn failure(&self, error: ProviderError) -> StreamFailure {
        StreamFailure {
            error,
            partial: self.partial(),
        }
    }

    // StreamFailure intentionally owns the partial output so callers can resume
    // without a public contract change.
    #[allow(clippy::result_large_err)]
    fn finish(self) -> std::result::Result<TurnOutput, StreamFailure> {
        let partial = self.partial();
        let content = self
            .blocks
            .into_values()
            .map(BlockBuilder::finalize)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| StreamFailure {
                error,
                partial: partial.clone(),
            })?;
        Ok(TurnOutput { content, ..partial })
    }
}

#[derive(Debug)]
enum BlockBuilder {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
}

impl BlockBuilder {
    fn finalize(self) -> std::result::Result<ContentBlock, ProviderError> {
        match self {
            BlockBuilder::Text(text) => Ok(ContentBlock::Text {
                text,
                cache_control: None,
            }),
            BlockBuilder::ToolUse {
                id,
                name,
                input_json,
            } => {
                let input = if input_json.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&input_json).map_err(|error| {
                        ProviderError::invalid_response(format!(
                            "invalid incremental tool arguments for {name}: {error}"
                        ))
                    })?
                };
                Ok(ContentBlock::ToolUse { id, name, input })
            }
            BlockBuilder::Thinking {
                thinking,
                signature,
            } => Ok(ContentBlock::Thinking {
                thinking,
                signature,
            }),
        }
    }

    fn finalize_lossy(&self) -> ContentBlock {
        match self {
            BlockBuilder::Text(text) => ContentBlock::Text {
                text: text.clone(),
                cache_control: None,
            },
            BlockBuilder::ToolUse {
                id,
                name,
                input_json,
            } => ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: serde_json::from_str(input_json)
                    .unwrap_or_else(|_| serde_json::json!({"_partial_json": input_json})),
            },
            BlockBuilder::Thinking {
                thinking,
                signature,
            } => ContentBlock::Thinking {
                thinking: thinking.clone(),
                signature: signature.clone(),
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_frame(
    frame: &crate::sse::SseFrame,
    message_id: &mut String,
    model: &mut String,
    usage: &mut Usage,
    stop_reason: &mut Option<StopReason>,
    blocks: &mut BTreeMap<usize, BlockBuilder>,
    on_event: &mut impl FnMut(&StreamEvent),
) -> Result<()> {
    match frame.event.as_str() {
        "message_start" => {
            #[derive(Deserialize)]
            struct P {
                message: M,
            }
            #[derive(Deserialize)]
            struct M {
                id: String,
                #[serde(default)]
                model: String,
                #[serde(default)]
                usage: UsagePart,
            }
            if let Ok(p) = serde_json::from_str::<P>(&frame.data) {
                *message_id = p.message.id.clone();
                *model = p.message.model.clone();
                usage.update_from_part(&p.message.usage);
                on_event(&StreamEvent::MessageStart {
                    message_id: p.message.id,
                    model: p.message.model,
                    usage: p.message.usage,
                });
            }
        }
        "content_block_start" => {
            #[derive(Deserialize)]
            struct P {
                index: usize,
                content_block: B,
            }
            #[derive(Deserialize)]
            struct B {
                #[serde(rename = "type")]
                kind: String,
                #[serde(default)]
                id: Option<String>,
                #[serde(default)]
                name: Option<String>,
                #[serde(default)]
                text: Option<String>,
                #[serde(default)]
                thinking: Option<String>,
            }
            if let Ok(p) = serde_json::from_str::<P>(&frame.data) {
                let b = match p.content_block.kind.as_str() {
                    "tool_use" => BlockBuilder::ToolUse {
                        id: p.content_block.id.unwrap_or_default(),
                        name: p.content_block.name.unwrap_or_default(),
                        input_json: String::new(),
                    },
                    "thinking" | "redacted_thinking" => BlockBuilder::Thinking {
                        thinking: p.content_block.thinking.unwrap_or_default(),
                        signature: None,
                    },
                    _ => BlockBuilder::Text(p.content_block.text.unwrap_or_default()),
                };
                if let BlockBuilder::ToolUse { id, name, .. } = &b {
                    on_event(&StreamEvent::ToolUseStart {
                        index: p.index,
                        id: id.clone(),
                        name: name.clone(),
                    });
                }
                blocks.insert(p.index, b);
            }
        }
        "content_block_delta" => {
            #[derive(Deserialize)]
            struct P {
                index: usize,
                delta: D,
            }
            #[derive(Deserialize)]
            struct D {
                #[serde(rename = "type")]
                kind: String,
                #[serde(default)]
                text: Option<String>,
                #[serde(default)]
                partial_json: Option<String>,
                #[serde(default)]
                thinking: Option<String>,
                #[serde(default)]
                signature: Option<String>,
            }
            if let Ok(p) = serde_json::from_str::<P>(&frame.data) {
                match p.delta.kind.as_str() {
                    "text_delta" => {
                        if let Some(t) = p.delta.text {
                            on_event(&StreamEvent::TextDelta { text: t.clone() });
                            if let Some(BlockBuilder::Text(s)) = blocks.get_mut(&p.index) {
                                s.push_str(&t);
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(pj) = p.delta.partial_json {
                            on_event(&StreamEvent::ToolUseInputDelta {
                                index: p.index,
                                partial_json: pj.clone(),
                            });
                            if let Some(BlockBuilder::ToolUse { input_json, .. }) =
                                blocks.get_mut(&p.index)
                            {
                                input_json.push_str(&pj);
                            }
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = p.delta.thinking {
                            on_event(&StreamEvent::ThinkingDelta {
                                thinking: t.clone(),
                            });
                            if let Some(BlockBuilder::Thinking { thinking, .. }) =
                                blocks.get_mut(&p.index)
                            {
                                thinking.push_str(&t);
                            }
                        }
                    }
                    "signature_delta" => {
                        if let Some(sig) = p.delta.signature {
                            if let Some(BlockBuilder::Thinking { signature, .. }) =
                                blocks.get_mut(&p.index)
                            {
                                *signature = Some(sig);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            #[derive(Deserialize)]
            struct P {
                index: usize,
            }
            if let Ok(p) = serde_json::from_str::<P>(&frame.data) {
                on_event(&StreamEvent::BlockStop { index: p.index });
            }
        }
        "message_delta" => {
            #[derive(Deserialize)]
            struct P {
                delta: D,
                #[serde(default)]
                usage: Option<UsagePart>,
            }
            #[derive(Deserialize)]
            struct D {
                #[serde(default)]
                stop_reason: Option<StopReason>,
            }
            if let Ok(p) = serde_json::from_str::<P>(&frame.data) {
                if let Some(u) = &p.usage {
                    usage.update_from_part(u);
                }
                if p.delta.stop_reason.is_some() {
                    *stop_reason = p.delta.stop_reason.clone();
                }
                on_event(&StreamEvent::MessageDelta {
                    stop_reason: p.delta.stop_reason,
                    usage: p.usage.unwrap_or_default(),
                });
            }
        }
        "message_stop" => {
            on_event(&StreamEvent::MessageStop);
        }
        "error" => {
            return Err(api_error_from_body(0, &frame.data));
        }
        _ => {}
    }
    Ok(())
}

/// Build the streaming request body JSON.
fn serialize_body_anthropic(params: &RequestParams) -> Result<String> {
    let mut body = serde_json::json!({
        "model": params.model,
        "max_tokens": params.max_tokens,
        "stream": true,
        "messages": serde_json::to_value(&params.messages)?,
    });
    if !params.system.is_empty() {
        body["system"] = serde_json::to_value(&params.system)?;
    }
    if !params.tools.is_empty() {
        body["tools"] = serde_json::to_value(&params.tools)?;
    }
    if let Some(tc) = &params.tool_choice {
        body["tool_choice"] = serde_json::to_value(tc)?;
    }
    if let Some(thinking) = &params.thinking {
        body["thinking"] = serde_json::to_value(thinking)?;
    }
    if let Some(t) = params.temperature {
        body["temperature"] = t.into();
    }
    Ok(serde_json::to_string(&body)?)
}

fn role_str(role: nonoclaw_core::Role) -> &'static str {
    match role {
        nonoclaw_core::Role::User => "user",
        nonoclaw_core::Role::Assistant => "assistant",
    }
}

/// Serialize in OpenAI Chat Completions format.
/// Converts Anthropic-style content blocks to OpenAI format on the fly.
fn serialize_body_openai(params: &RequestParams) -> Result<String> {
    use nonoclaw_core::ContentBlock;
    use nonoclaw_core::MessageContent;

    // Convert messages: Anthropic → OpenAI format
    let mut messages: Vec<serde_json::Value> = Vec::new();

    // System prompt becomes a system message.
    for block in &params.system {
        messages.push(serde_json::json!({
            "role": "system",
            "content": block.text,
        }));
    }

    // Convert each message — process entire Messages, not individual blocks.
    for msg in &params.messages {
        match &msg.content {
            MessageContent::Text(s) => {
                let role = role_str(msg.role);
                messages.push(serde_json::json!({"role": role, "content": s}));
            }
            MessageContent::Blocks(blocks) => {
                // Separate blocks by kind: Text/Image vs ToolUse vs ToolResult
                let mut text_parts: Vec<serde_json::Value> = Vec::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                let mut tool_results: Vec<serde_json::Value> = Vec::new();

                for b in blocks {
                    match b {
                        ContentBlock::Text { text, .. } => {
                            text_parts.push(serde_json::json!({"type":"text","text":text}));
                        }
                        ContentBlock::Image { source } => {
                            text_parts.push(serde_json::json!({
                                "type":"image_url",
                                "image_url":{"url": format!("data:{};base64,{}", source.media_type, source.data)}
                            }));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(serde_json::json!({
                                "id": id, "type": "function",
                                "function": {"name": name, "arguments": serde_json::to_string(input).unwrap_or_default()}
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            let text = match content {
                                nonoclaw_core::ToolResultContent::Text(s) => s.clone(),
                                _ => String::new(),
                            };
                            tool_results.push(serde_json::json!({
                                "role":"tool", "tool_call_id": tool_use_id, "content": text
                            }));
                        }
                        ContentBlock::Thinking { .. } => {}
                    }
                }

                // Combine text parts: single text → plain string;
                // multiple parts (text + image) → content array.
                let content_val = if text_parts.is_empty() {
                    None
                } else if text_parts.len() == 1 && text_parts[0].get("text").is_some() {
                    // Plain text → send as string, not {"type":"text","text":"..."}
                    text_parts[0]["text"].as_str().map(|s| serde_json::json!(s))
                } else {
                    Some(serde_json::json!(text_parts))
                };

                match msg.role {
                    nonoclaw_core::Role::User => {
                        for tr in &tool_results {
                            messages.push(tr.clone());
                        }
                        if let Some(c) = content_val {
                            messages.push(serde_json::json!({"role":"user","content":c}));
                        }
                    }
                    nonoclaw_core::Role::Assistant => {
                        if !tool_calls.is_empty() {
                            let mut m =
                                serde_json::json!({"role":"assistant","tool_calls":tool_calls});
                            if let Some(c) = &content_val {
                                m["content"] = c.clone();
                            }
                            messages.push(m);
                        } else if let Some(c) = content_val {
                            messages.push(serde_json::json!({"role":"assistant","content":c}));
                        }
                    }
                }
            }
        }
    }

    // Convert tool definitions to OpenAI format.
    let tools: Vec<serde_json::Value> = params
        .tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect();

    let mut body = serde_json::json!({
        "model": params.model,
        "max_tokens": params.max_tokens,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if !tools.is_empty() {
        body["tools"] = serde_json::json!(tools);
        body["tool_choice"] = match params.tool_choice.as_ref() {
            None | Some(ToolChoice::Auto) => serde_json::json!("auto"),
            Some(ToolChoice::Any) => serde_json::json!("required"),
            Some(ToolChoice::Tool { name }) => serde_json::json!({
                "type": "function",
                "function": {"name": name}
            }),
            Some(ToolChoice::None) => serde_json::json!("none"),
        };
    }
    if let Some(t) = params.temperature {
        body["temperature"] = serde_json::json!(t);
    }

    Ok(serde_json::to_string(&body)?)
}

/// OpenAI usage fields use different names than Anthropic.
#[derive(Debug, Clone, Default, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: OpenAiPromptTokenDetails,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OpenAiPromptTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Default)]
struct OpenAiToolBuilder {
    id: String,
    name: String,
    arguments: String,
    start_emitted: bool,
}

#[derive(Debug, Default)]
struct OpenAiState {
    message_id: String,
    model: String,
    text: String,
    tools: BTreeMap<usize, OpenAiToolBuilder>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    message_started: bool,
}

impl OpenAiState {
    fn partial(&self) -> TurnOutput {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::Text {
                text: self.text.clone(),
                cache_control: None,
            });
        }
        content.extend(self.tools.values().map(|tool| {
            ContentBlock::ToolUse {
                id: tool.id.clone(),
                name: tool.name.clone(),
                input: serde_json::from_str(&tool.arguments)
                    .unwrap_or_else(|_| serde_json::json!({"_partial_json": tool.arguments})),
            }
        }));
        TurnOutput {
            message_id: self.message_id.clone(),
            model: self.model.clone(),
            content,
            stop_reason: self.stop_reason.clone(),
            usage: self.usage,
        }
    }

    fn failure(&self, error: ProviderError) -> StreamFailure {
        StreamFailure {
            error,
            partial: self.partial(),
        }
    }

    // StreamFailure intentionally owns the partial output so callers can resume
    // without a public contract change.
    #[allow(clippy::result_large_err)]
    fn finish(self) -> std::result::Result<TurnOutput, StreamFailure> {
        let partial = self.partial();
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::Text {
                text: self.text,
                cache_control: None,
            });
        }
        for tool in self.tools.into_values() {
            let input = if tool.arguments.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&tool.arguments).map_err(|error| StreamFailure {
                    error: ProviderError::invalid_response(format!(
                        "invalid incremental tool arguments for {}: {error}",
                        tool.name
                    )),
                    partial: partial.clone(),
                })?
            };
            content.push(ContentBlock::ToolUse {
                id: tool.id,
                name: tool.name,
                input,
            });
        }
        Ok(TurnOutput { content, ..partial })
    }
}

async fn fold_openai_stream(
    response: reqwest::Response,
    on_event: &mut impl FnMut(&StreamEvent),
    cancel: &CancellationToken,
) -> std::result::Result<TurnOutput, StreamFailure> {
    let mut parser = SseParser::new();
    let mut stream = response.bytes_stream();
    let mut state = OpenAiState::default();

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(state.failure(ProviderError::cancelled())),
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else { break };
        let bytes =
            chunk.map_err(|error| state.failure(ProviderError::stream(error.to_string(), true)))?;
        parser.feed_bytes(&bytes);
        while let Some(frame) = parser.next_frame() {
            if frame.data.trim() == "[DONE]" {
                on_event(&StreamEvent::MessageStop);
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(&frame.data).map_err(|error| {
                state.failure(ProviderError::invalid_response(format!(
                    "invalid OpenAI SSE JSON: {error}"
                )))
            })?;
            if value.get("error").is_some() {
                let error = api_error_from_body(0, &frame.data);
                return Err(state.failure(ProviderError::from_core(&error, "read_stream")));
            }
            handle_openai_chunk(&value, &mut state, on_event)?;
        }
    }

    state.finish()
}

#[allow(clippy::result_large_err)]
fn handle_openai_chunk(
    value: &serde_json::Value,
    state: &mut OpenAiState,
    on_event: &mut impl FnMut(&StreamEvent),
) -> std::result::Result<(), StreamFailure> {
    if let Some(id) = value.get("id").and_then(|value| value.as_str()) {
        state.message_id = id.to_string();
    }
    if let Some(model) = value.get("model").and_then(|value| value.as_str()) {
        state.model = model.to_string();
    }
    if !state.message_started {
        state.message_started = true;
        on_event(&StreamEvent::MessageStart {
            message_id: state.message_id.clone(),
            model: state.model.clone(),
            usage: UsagePart::default(),
        });
    }

    if let Some(raw_usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        let usage = serde_json::from_value::<OpenAiUsage>(raw_usage.clone()).map_err(|error| {
            state.failure(ProviderError::invalid_response(format!(
                "invalid OpenAI usage: {error}"
            )))
        })?;
        let part = UsagePart {
            input_tokens: Some(usage.prompt_tokens),
            output_tokens: Some(usage.completion_tokens),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(usage.prompt_tokens_details.cached_tokens),
        };
        state.usage.update_from_part(&part);
        on_event(&StreamEvent::MessageDelta {
            stop_reason: None,
            usage: part,
        });
    }

    let Some(choices) = value.get("choices").and_then(|value| value.as_array()) else {
        return Ok(());
    };
    for choice in choices {
        if let Some(reason) = choice.get("finish_reason").and_then(|value| value.as_str()) {
            let reason = openai_stop_reason(reason);
            state.stop_reason = Some(reason.clone());
            on_event(&StreamEvent::MessageDelta {
                stop_reason: Some(reason),
                usage: UsagePart::default(),
            });
        }
        let delta = &choice["delta"];
        if let Some(text) = delta.get("content").and_then(|value| value.as_str()) {
            if !text.is_empty() {
                state.text.push_str(text);
                on_event(&StreamEvent::TextDelta {
                    text: text.to_string(),
                });
            }
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|value| value.as_array()) {
            for tool_call in tool_calls {
                let index = tool_call
                    .get("index")
                    .and_then(|value| value.as_u64())
                    .unwrap_or_default() as usize;
                let tool = state.tools.entry(index).or_default();
                if let Some(id) = tool_call.get("id").and_then(|value| value.as_str()) {
                    tool.id.push_str(id);
                }
                if let Some(name) = tool_call
                    .pointer("/function/name")
                    .and_then(|value| value.as_str())
                {
                    tool.name.push_str(name);
                }
                if !tool.start_emitted && (!tool.id.is_empty() || !tool.name.is_empty()) {
                    tool.start_emitted = true;
                    on_event(&StreamEvent::ToolUseStart {
                        index,
                        id: tool.id.clone(),
                        name: tool.name.clone(),
                    });
                }
                if let Some(arguments) = tool_call
                    .pointer("/function/arguments")
                    .and_then(|value| value.as_str())
                {
                    tool.arguments.push_str(arguments);
                    on_event(&StreamEvent::ToolUseInputDelta {
                        index,
                        partial_json: arguments.to_string(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn openai_stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" | "end_turn" => StopReason::EndTurn,
        "length" | "max_tokens" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}

/// Prompt diagnostics are opt-in and contain structure/size metadata only.
/// Raw system text, user messages, tool descriptions, hidden reasoning, and
/// attachment bytes are never written, even when diagnostics are enabled.
fn prompt_log_requested(value: Option<&str>) -> bool {
    value.is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn write_prompt_log(params: &RequestParams, _body: &str, _url: &str) {
    let setting = std::env::var("NONOCLAW_PROMPT_LOG").ok();
    if !prompt_log_requested(setting.as_deref()) {
        return;
    }
    static WARNING: std::sync::Once = std::sync::Once::new();
    WARNING.call_once(|| {
        tracing::warn!(
            "NONOCLAW_PROMPT_LOG is enabled; only redacted prompt metadata will be written"
        );
    });

    let trace = params.trace_label.as_deref().unwrap_or("unknown");
    if trace.starts_with("hook-") {
        tracing::debug!(
            trace,
            "hook prompt log omitted (sensitive payload redacted)"
        );
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let log_dir = cwd.join(".nonoclaw/logs/prompts");
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        tracing::warn!(kind = ?error.kind(), "cannot create prompt log directory");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) =
            std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o700))
        {
            tracing::warn!(kind = ?error.kind(), "cannot secure prompt log directory");
            return;
        }
    }
    let safe: String = trace
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .take(96)
        .collect();
    let path = log_dir.join(format!("{safe}.json"));
    let pretty = serde_json::to_string_pretty(&redacted_prompt_metadata(params))
        .unwrap_or_else(|_| "{\"error\":\"redacted prompt metadata unavailable\"}".into());

    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            use std::io::Write;
            if let Err(error) = file.write_all(pretty.as_bytes()) {
                tracing::warn!(kind = ?error.kind(), "failed to write redacted prompt log");
            }
        }
        Err(error) => tracing::warn!(kind = ?error.kind(), "failed to open redacted prompt log"),
    }
}

fn redacted_prompt_metadata(params: &RequestParams) -> serde_json::Value {
    let messages = params
        .messages
        .iter()
        .map(|message| {
            let (kind, chars, attachments) = match &message.content {
                nonoclaw_core::MessageContent::Text(text) => ("text", text.chars().count(), 0),
                nonoclaw_core::MessageContent::Blocks(blocks) => {
                    let chars = blocks
                        .iter()
                        .map(|block| match block {
                            nonoclaw_core::ContentBlock::Text { text, .. } => text.chars().count(),
                            _ => 0,
                        })
                        .sum();
                    let attachments = blocks
                        .iter()
                        .filter(|block| matches!(block, nonoclaw_core::ContentBlock::Image { .. }))
                        .count();
                    ("blocks", chars, attachments)
                }
            };
            serde_json::json!({
                "role": message.role,
                "content": "[REDACTED PROMPT CONTENT]",
                "content_kind": kind,
                "text_chars": chars,
                "attachment_count": attachments
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "format": "nonoclaw-redacted-prompt-v1",
        "warning": "Prompt content, hidden reasoning, credentials, and attachments are omitted.",
        "trace": params.trace_label,
        "model": params.model,
        "max_tokens": params.max_tokens,
        "system": params.system.iter().map(|block| serde_json::json!({
            "content": "[REDACTED SYSTEM PROMPT]",
            "text_chars": block.text.chars().count(),
            "cached": block.cache_control.is_some()
        })).collect::<Vec<_>>(),
        "messages": messages,
        "tools": params.tools.iter().map(|tool| serde_json::json!({
            "name": tool.name,
            "description": "[REDACTED TOOL PROMPT]"
        })).collect::<Vec<_>>()
    })
}

// ── Prompt dump (structured logging) ────────────────────────────────────────

fn dump_prompt_anthropic(params: &RequestParams, _body: &str) {
    let sep = "═".repeat(72);
    let thin = "─".repeat(72);

    // ── System blocks ──
    let mut sys_chars = 0usize;
    let mut block1 = String::new();
    let mut block2 = String::new();
    for (i, b) in params.system.iter().enumerate() {
        let label = if b.cache_control.is_some() {
            "BLOCK #1  [CACHED · ephemeral]"
        } else {
            "BLOCK #2  [UNCACHED]"
        };
        let chars = b.text.chars().count();
        sys_chars += chars;
        let _preview = b.text.lines().take(8).collect::<Vec<_>>().join("\n");
        if i == 0 {
            block1 = format!("  {label:36} {chars:>6} chars");
        } else {
            block2 = format!("  {label:36} {chars:>6} chars");
        }
    }

    // ── Tools ──
    let tools_chars: usize = params
        .tools
        .iter()
        .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
        .sum();
    let tool_names: Vec<&str> = params.tools.iter().map(|t| t.name.as_str()).collect();

    // ── Messages ──
    let msg_chars: usize = params
        .messages
        .iter()
        .map(|m| match &m.content {
            nonoclaw_core::MessageContent::Text(s) => s.chars().count(),
            nonoclaw_core::MessageContent::Blocks(bs) => bs
                .iter()
                .map(|b| match b {
                    nonoclaw_core::ContentBlock::Text { text, .. } => text.chars().count(),
                    nonoclaw_core::ContentBlock::Image { .. } => 1200,
                    _ => 200,
                })
                .sum(),
        })
        .sum();
    let est_tokens = (sys_chars + tools_chars + msg_chars) / 4 + params.messages.len() * 4;
    let trace = params.trace_label.as_deref().unwrap_or("?");

    tracing::info!(
        "\n{sep}\n\
         📤  TURN REQUEST  [{trace}]\n\
         {sep}\n\
         Model:     {model}\n\
         Max out:   {max_tok} tokens\n\
         Sessions:  {n_msgs} messages · ~{est} est. tokens\n\
         {thin}\n\
         {b1}\n\
         {b2}\n\
         {thin}\n\
           TOOLS   · {n_tools:>2} total · {t_chars:>6} chars  {t_names}\n\
         {thin}\n\
           MESSAGES · {n_msgs:>2} total · {m_chars:>6} chars\n\
         {thin}\n\
         Message roles: {roles}\n\
         {sep}",
        model = params.model,
        max_tok = params.max_tokens,
        n_msgs = params.messages.len(),
        est = est_tokens,
        b1 = block1,
        b2 = block2,
        n_tools = params.tools.len(),
        t_chars = tools_chars,
        t_names = tool_names.join(", "),
        m_chars = msg_chars,
        roles = params
            .messages
            .iter()
            .map(|m| match m.role {
                nonoclaw_core::Role::User => "U",
                nonoclaw_core::Role::Assistant => "A",
            })
            .collect::<Vec<_>>()
            .join(" "),
    );
}

fn dump_prompt_openai(params: &RequestParams, _body: &str) {
    let sep = "═".repeat(72);
    let thin = "─".repeat(72);

    let sys_chars: usize = params.system.iter().map(|b| b.text.chars().count()).sum();
    // Per-block breakdown for debugging large system prompts.
    let block_sizes: Vec<String> = params
        .system
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let cached = if b.cache_control.is_some() {
                "[CACHED]"
            } else {
                "[UNCACHED]"
            };
            let lines = b.text.lines().count();
            format!(
                "Block#{} {} {}chars {}lines",
                i + 1,
                cached,
                b.text.chars().count(),
                lines
            )
        })
        .collect();

    let msg_chars: usize = params
        .messages
        .iter()
        .map(|m| match &m.content {
            nonoclaw_core::MessageContent::Text(s) => s.chars().count(),
            nonoclaw_core::MessageContent::Blocks(bs) => bs
                .iter()
                .map(|b| match b {
                    nonoclaw_core::ContentBlock::Text { text, .. } => text.chars().count(),
                    nonoclaw_core::ContentBlock::Image { .. } => 1200,
                    _ => 200,
                })
                .sum(),
        })
        .sum();
    let est_tokens = (sys_chars + msg_chars) / 4 + params.messages.len() * 4;

    let img_count: usize = params
        .messages
        .iter()
        .map(|m| match &m.content {
            nonoclaw_core::MessageContent::Blocks(bs) => bs
                .iter()
                .filter(|b| matches!(b, nonoclaw_core::ContentBlock::Image { .. }))
                .count(),
            _ => 0,
        })
        .sum();

    let trace = params.trace_label.as_deref().unwrap_or("?");
    let block_detail = block_sizes.join("\n  ");

    tracing::info!(
        "\n{sep}\n\
         📤  TURN REQUEST  [OpenAI] [{trace}]\n\
         {sep}\n\
         Model:     {model}\n\
         Max out:   {max_tok} tokens\n\
         {thin}\n\
           SYSTEM   · {s_chars:>6} chars ({n_sys} blocks):\n  {blocks}\n\
         {thin}\n\
           MESSAGES · {n_msgs:>2} total · {m_chars:>6} chars · {img} images\n\
         {thin}\n\
         Message roles: {roles}\n\
         ~{est} est. tokens\n\
         {sep}",
        model = params.model,
        max_tok = params.max_tokens,
        s_chars = sys_chars,
        n_sys = params.system.len(),
        blocks = block_detail,
        n_msgs = params.messages.len(),
        m_chars = msg_chars,
        img = img_count,
        est = est_tokens,
        roles = params
            .messages
            .iter()
            .map(|m| match m.role {
                nonoclaw_core::Role::User => "U",
                nonoclaw_core::Role::Assistant => "A",
            })
            .collect::<Vec<_>>()
            .join(" "),
    );
}

/// Translate a non-2xx response (or `event: error` payload) into an [`Error`],
/// classifying retryability. Mirrors `src/services/api/errors.ts`.
pub(crate) fn api_error_from_body(status: u16, text: &str) -> Error {
    // Body shape: {"type":"error","error":{"type":"overloaded_error","message":"..."}}
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(err) = v.get("error").and_then(|e| e.as_object()) {
            let etype = err
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            if status == 401
                || status == 403
                || etype == "authentication_error"
                || etype == "invalid_api_key"
            {
                return Error::Auth(if msg.is_empty() {
                    format!("status {status}")
                } else {
                    msg
                });
            }
            if etype == "overloaded_error" || status == 529 {
                return Error::Overloaded(msg);
            }
            if msg.contains("prompt is too long") || etype == "prompt_too_long" || status == 400 {
                // 400s are generally non-retryable; detect prompt-too-long explicitly.
                if etype == "prompt_too_long" || msg.contains("prompt is too long") {
                    return Error::PromptTooLong(msg);
                }
            }
            let kind = Error::classify_status(status);
            return Error::Api {
                status,
                message: format!("{etype}: {msg}"),
                kind,
            };
        }
    }
    if status == 401 || status == 403 {
        return Error::Auth(format!("status {status}: {text}"));
    }
    Error::Api {
        status,
        message: text.to_string(),
        kind: Error::classify_status(status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseFrame;

    #[test]
    fn folds_text_and_tool_use_from_recorded_sse() {
        // A recorded stream: text + a tool_use whose input arrives in two
        // partial_json fragments, then end_turn.
        let frames: Vec<SseFrame> = vec![
            SseFrame {
                event: "message_start".into(),
                data: r#"{"message":{"id":"msg_1","usage":{"input_tokens":50,"output_tokens":1}}}"#
                    .into(),
            },
            SseFrame {
                event: "content_block_start".into(),
                data: r#"{"index":0,"content_block":{"type":"text","text":""}}"#.into(),
            },
            SseFrame {
                event: "content_block_delta".into(),
                data: r#"{"index":0,"delta":{"type":"text_delta","text":"Hello "}}"#.into(),
            },
            SseFrame {
                event: "content_block_delta".into(),
                data: r#"{"index":0,"delta":{"type":"text_delta","text":"world"}}"#.into(),
            },
            SseFrame {
                event: "content_block_stop".into(),
                data: r#"{"index":0}"#.into(),
            },
            SseFrame {
                event: "content_block_start".into(),
                data: r#"{"index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"Read"}}"#
                    .into(),
            },
            SseFrame {
                event: "content_block_delta".into(),
                data: r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_"}}"#
                    .into(),
            },
            SseFrame {
                event: "content_block_delta".into(),
                data: r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"path\":\"/a\"}"}}"#
                    .into(),
            },
            SseFrame {
                event: "content_block_stop".into(),
                data: r#"{"index":1}"#.into(),
            },
            SseFrame {
                event: "message_delta".into(),
                data: r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#.into(),
            },
            SseFrame {
                event: "message_stop".into(),
                data: "{}".into(),
            },
        ];

        let mut message_id = String::new();
        let mut model = String::new();
        let mut usage = Usage::default();
        let mut stop_reason: Option<StopReason> = None;
        let mut blocks: BTreeMap<usize, BlockBuilder> = BTreeMap::new();
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut cb = |e: &StreamEvent| events.push(e.clone());

        for f in &frames {
            handle_frame(
                f,
                &mut message_id,
                &mut model,
                &mut usage,
                &mut stop_reason,
                &mut blocks,
                &mut cb,
            )
            .unwrap();
        }

        assert_eq!(message_id, "msg_1");
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 42);
        assert_eq!(stop_reason, Some(StopReason::ToolUse));

        let content: Vec<ContentBlock> = blocks
            .into_values()
            .map(|b| b.finalize().unwrap())
            .collect();
        assert_eq!(content.len(), 2);
        match &content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "Hello world"),
            _ => panic!("expected text block"),
        }
        match &content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "/a");
            }
            _ => panic!("expected tool_use block"),
        }
        // Text deltas were streamed live.
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text } if text == "world")));
    }

    #[test]
    fn classifies_overloaded_as_retryable() {
        let e = api_error_from_body(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
        );
        assert!(e.is_retryable());
        assert!(matches!(e, Error::Overloaded(_)));
    }

    #[test]
    fn classifies_prompt_too_long() {
        let e = api_error_from_body(
            400,
            r#"{"type":"error","error":{"type":"prompt_too_long","message":"prompt is too long"}}"#,
        );
        assert!(matches!(e, Error::PromptTooLong(_)));
        assert!(!e.is_retryable());
    }

    fn fixture_params() -> RequestParams {
        RequestParams {
            model: "fixture-model".into(),
            max_tokens: 128,
            system: vec![],
            messages: vec![Message::user(nonoclaw_core::MessageContent::from_blocks(
                vec![
                    ContentBlock::Text {
                        text: "inspect".into(),
                        cache_control: None,
                    },
                    ContentBlock::Image {
                        source: nonoclaw_core::ImageSource {
                            kind: "base64".into(),
                            media_type: "image/png".into(),
                            data: "aW1hZ2U=".into(),
                        },
                    },
                ],
            ))],
            tools: vec![ToolSchema {
                name: "Read".into(),
                description: "read a file".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"file_path": {"type": "string"}}
                }),
                cache_control: None,
            }],
            tool_choice: Some(ToolChoice::Auto),
            thinking: None,
            temperature: None,
            betas: vec![],
            trace_label: Some("provider-fixture".into()),
        }
    }

    #[test]
    fn anthropic_fixture_preserves_thinking_cache_usage_and_tools() {
        let mut parser = SseParser::new();
        parser.feed_str(include_str!("../tests/fixtures/anthropic_stream.sse"));
        let mut state = AnthropicState::default();
        let mut events = Vec::new();
        while let Some(frame) = parser.next_frame() {
            handle_frame(
                &frame,
                &mut state.message_id,
                &mut state.model,
                &mut state.usage,
                &mut state.stop_reason,
                &mut state.blocks,
                &mut |event| events.push(event.clone()),
            )
            .unwrap();
        }
        let output = state.finish().unwrap();
        assert_eq!(output.model, "claude-fixture");
        assert_eq!(output.usage.cache_creation_input_tokens, 7);
        assert_eq!(output.usage.cache_read_input_tokens, 5);
        assert!(matches!(output.content[0], ContentBlock::Thinking { .. }));
        assert!(matches!(
            &output.content[2],
            ContentBlock::ToolUse { input, .. } if input["file_path"] == "/tmp/a"
        ));
        assert!(events.iter().any(
            |event| matches!(event, StreamEvent::ThinkingDelta { thinking } if thinking == "checking")
        ));
    }

    #[test]
    fn openai_fixture_streams_text_incremental_tool_arguments_and_usage() {
        let mut parser = SseParser::new();
        parser.feed_str(include_str!("../tests/fixtures/openai_stream.sse"));
        let mut state = OpenAiState::default();
        let mut events = Vec::new();
        while let Some(frame) = parser.next_frame() {
            if frame.data.trim() != "[DONE]" {
                let value: serde_json::Value = serde_json::from_str(&frame.data).unwrap();
                handle_openai_chunk(&value, &mut state, &mut |event| events.push(event.clone()))
                    .unwrap();
            }
        }
        let output = state.finish().unwrap();
        assert_eq!(output.model, "gpt-fixture");
        assert_eq!(output.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(output.usage.input_tokens, 12);
        assert_eq!(output.usage.output_tokens, 8);
        assert_eq!(output.usage.cache_read_input_tokens, 4);
        assert!(matches!(
            &output.content[0],
            ContentBlock::Text { text, .. } if text == "Hello"
        ));
        assert!(matches!(
            &output.content[1],
            ContentBlock::ToolUse { input, .. } if input["file_path"] == "/tmp/a"
        ));
        let argument_deltas = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ToolUseInputDelta { .. }))
            .count();
        assert_eq!(argument_deltas, 2);
    }

    #[test]
    fn request_formats_cover_images_tools_and_anthropic_prompt_caching() {
        let mut params = fixture_params();
        let openai: serde_json::Value =
            serde_json::from_str(&serialize_body_openai(&params).unwrap()).unwrap();
        assert_eq!(openai["stream"], true);
        assert_eq!(openai["stream_options"]["include_usage"], true);
        assert_eq!(openai["messages"][0]["content"][1]["type"], "image_url");
        assert_eq!(openai["tools"][0]["function"]["name"], "Read");

        params.system.push(SystemBlock {
            kind: "text".into(),
            text: "cached system".into(),
            cache_control: Some(CacheControl {
                kind: nonoclaw_core::CacheControlKind::Ephemeral,
            }),
        });
        params.tools[0].cache_control = Some(CacheControl {
            kind: nonoclaw_core::CacheControlKind::Ephemeral,
        });
        let anthropic: serde_json::Value =
            serde_json::from_str(&serialize_body_anthropic(&params).unwrap()).unwrap();
        assert_eq!(anthropic["stream"], true);
        assert_eq!(anthropic["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(anthropic["tools"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn unsupported_openai_capabilities_are_explicit_before_network_io() {
        let client = Client::new(
            Some("fixture-key".into()),
            None,
            "http://127.0.0.1:1".into(),
        )
        .unwrap()
        .with_format(ApiFormat::OpenAI);
        assert!(!client.capabilities().thinking.is_supported());
        assert!(!client.capabilities().prompt_caching.is_supported());
        let mut params = fixture_params();
        params.thinking = Some(ThinkingConfig::adaptive());
        let error = client.validate_capabilities(&params).unwrap_err();
        assert_eq!(error.code, crate::ProviderErrorCode::Capability);
        assert_eq!(error.feature, Some(ProviderFeature::Thinking));
    }

    #[test]
    fn normalizes_anthropic_and_openai_error_fixtures() {
        let overloaded =
            api_error_from_body(529, include_str!("../tests/fixtures/anthropic_error.json"));
        assert!(matches!(overloaded, Error::Overloaded(_)));
        assert!(overloaded.is_retryable());

        let rate_limit =
            api_error_from_body(429, include_str!("../tests/fixtures/openai_error.json"));
        let normalized = ProviderError::from_core(&rate_limit, "start_stream");
        assert_eq!(normalized.code, crate::ProviderErrorCode::RateLimit);
        assert!(normalized.retryable);
    }

    async fn spawn_chunked_fixture(
        first_sse: &'static str,
        malformed_tail: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let chunk = format!("{:X}\r\n{}\r\n", first_sse.len(), first_sse);
            socket.write_all(chunk.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            if malformed_tail {
                socket.write_all(b"20\r\ntruncated").await.unwrap();
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn pre_stream_retry_is_bounded_and_emits_trace_event() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                if attempt == 0 {
                    let body = include_str!("../tests/fixtures/anthropic_error.json");
                    let response = format!(
                        "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                } else {
                    let body = include_str!("../tests/fixtures/openai_stream.sse");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            }
        });
        let client = Client::new(
            Some("fixture-key".into()),
            None,
            format!("http://{address}"),
        )
        .unwrap()
        .with_format(ApiFormat::OpenAI)
        .with_retry(RetryConfig {
            max_attempts: 2,
            initial_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(1),
            max_elapsed: std::time::Duration::from_secs(1),
            jitter_percent: 20,
        });
        let mut events = Vec::new();
        let output = client
            .run_turn(&fixture_params(), |event| events.push(event.clone()))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(output.message_id, "chatcmpl_fixture");
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::RetryScheduled { attempt: 2, delay_ms, .. } if *delay_ms <= 1
        )));
    }

    #[tokio::test]
    async fn cancellation_preserves_received_partial_content() {
        let first = "data: {\"id\":\"cancelled\",\"model\":\"gpt-fixture\",\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
        let (base_url, server) = spawn_chunked_fixture(first, false).await;
        let client = Client::new(Some("fixture-key".into()), None, base_url)
            .unwrap()
            .with_format(ApiFormat::OpenAI);
        let cancel = CancellationToken::new();
        let cancel_from_event = cancel.clone();
        let failure = client
            .run_turn_with_cancel(
                &fixture_params(),
                move |event| {
                    if matches!(event, StreamEvent::TextDelta { .. }) {
                        cancel_from_event.cancel();
                    }
                },
                cancel,
            )
            .await
            .unwrap_err();
        server.abort();
        assert_eq!(failure.error.code, crate::ProviderErrorCode::Cancelled);
        assert!(matches!(
            &failure.partial.content[0],
            ContentBlock::Text { text, .. } if text == "partial"
        ));
    }

    #[tokio::test]
    async fn mid_stream_transport_failure_returns_structured_partial_output() {
        let first = "data: {\"id\":\"broken\",\"model\":\"gpt-fixture\",\"choices\":[{\"delta\":{\"content\":\"kept\"},\"finish_reason\":null}]}\n\n";
        let (base_url, server) = spawn_chunked_fixture(first, true).await;
        let client = Client::new(Some("fixture-key".into()), None, base_url)
            .unwrap()
            .with_format(ApiFormat::OpenAI);
        let mut events = Vec::new();
        let failure = client
            .run_turn_with_cancel(
                &fixture_params(),
                |event| events.push(event.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        server.await.unwrap();
        assert_eq!(failure.error.code, crate::ProviderErrorCode::Stream);
        assert!(failure.error.retryable);
        assert!(matches!(
            &failure.partial.content[0],
            ContentBlock::Text { text, .. } if text == "kept"
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::StreamError { partial, .. } if !partial.content.is_empty()
        )));
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use nonoclaw_core::MessageContent;

    #[test]
    fn prompt_diagnostic_metadata_omits_all_raw_content_and_secrets() {
        // **Validates: Requirements 9.8, 11.1**
        assert!(!prompt_log_requested(None));
        assert!(!prompt_log_requested(Some("0")));
        assert!(!prompt_log_requested(Some("false")));
        assert!(prompt_log_requested(Some("1")));
        assert!(prompt_log_requested(Some("TRUE")));

        let params = RequestParams {
            model: "fixture-model".into(),
            max_tokens: 42,
            system: vec![SystemBlock {
                kind: "text".into(),
                text: "system secret sk-proj-system".into(),
                cache_control: None,
            }],
            messages: vec![Message::user(MessageContent::from_text(
                "raw user prompt Bearer user-secret",
            ))],
            tools: vec![ToolSchema {
                name: "Read".into(),
                description: "secret tool instructions".into(),
                input_schema: serde_json::json!({"type":"object"}),
                cache_control: None,
            }],
            tool_choice: None,
            thinking: None,
            temperature: None,
            betas: vec![],
            trace_label: Some("fixture/../../trace".into()),
        };
        let encoded = redacted_prompt_metadata(&params).to_string();
        assert!(encoded.contains("fixture-model"));
        assert!(encoded.contains("REDACTED PROMPT CONTENT"));
        for forbidden in [
            "sk-proj-system",
            "raw user prompt",
            "user-secret",
            "secret tool instructions",
        ] {
            assert!(!encoded.contains(forbidden), "leaked {forbidden}");
        }
    }
}
