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

use crate::retry::{with_retry, RetryConfig};
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
    /// Optional trace label printed in prompt dump (e.g. "sess-abc:turn-3").
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
}

/// The fully-folded result of one streaming turn.
#[derive(Debug, Clone)]
pub struct TurnOutput {
    pub message_id: String,
    /// The real model reported by the API in `message_start` (empty if absent).
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFormat {
    Anthropic,
    OpenAI,
}

impl Default for ApiFormat {
    fn default() -> Self { ApiFormat::Anthropic }
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

    pub fn api_format(&self) -> ApiFormat { self.format }

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
    pub fn base_url(&self) -> &str { &self.base_url }
    /// The configured API key, if any.
    pub fn api_key(&self) -> Option<&str> { self.api_key.as_deref() }

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
                let url = format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'));
                (url, body)
            }
        };
        let mut req = self.http.post(url).header("content-type", "application/json");
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

    /// Execute one streaming turn, invoking `on_event` for each stream event
    /// and returning the folded [`TurnOutput`]. Retries on transient errors
    /// *before* the stream starts; mid-stream errors are terminal.
    pub async fn run_turn(
        &self,
        params: &RequestParams,
        mut on_event: impl FnMut(&StreamEvent),
    ) -> Result<TurnOutput> {
        let retry = self.retry.clone();
        let response = with_retry(&retry, || self.send_request(params)).await?;
        if self.format == ApiFormat::OpenAI {
            fold_openai_non_streaming(response, params, &mut on_event).await
        } else {
            fold_stream(response, &mut on_event).await
        }
    }
}

/// Fold the SSE response stream into a [`TurnOutput`], emitting events.
async fn fold_stream(
    response: reqwest::Response,
    on_event: &mut impl FnMut(&StreamEvent),
) -> Result<TurnOutput> {
    let mut parser = SseParser::new();
    let mut stream = response.bytes_stream();

    let mut message_id = String::new();
    let mut model = String::new();
    let mut usage = Usage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut blocks: BTreeMap<usize, BlockBuilder> = BTreeMap::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| Error::Network(e.to_string()))?;
        parser.feed_bytes(&bytes);
        while let Some(frame) = parser.next_frame() {
            handle_frame(
                &frame,
                &mut message_id,
                &mut model,
                &mut usage,
                &mut stop_reason,
                &mut blocks,
                on_event,
            )?;
        }
    }

    let content = blocks
        .into_values()
        .map(|b| b.finalize())
        .collect::<Result<Vec<_>>>()?;

    Ok(TurnOutput {
        message_id,
        model,
        content,
        stop_reason,
        usage,
    })
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
    fn finalize(self) -> Result<ContentBlock> {
        Ok(match self {
            BlockBuilder::Text(text) => ContentBlock::Text {
                text,
                cache_control: None,
            },
            BlockBuilder::ToolUse {
                id,
                name,
                input_json,
            } => {
                let input = if input_json.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&input_json).unwrap_or_else(|_| serde_json::json!({}))
                };
                ContentBlock::ToolUse { id, name, input }
            }
            BlockBuilder::Thinking {
                thinking,
                signature,
            } => ContentBlock::Thinking {
                thinking,
                signature,
            },
        })
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
                        ContentBlock::ToolResult { tool_use_id, content, .. } => {
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
                            let mut m = serde_json::json!({"role":"assistant","tool_calls":tool_calls});
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
    let tools: Vec<serde_json::Value> = params.tools.iter().map(|t| {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            }
        })
    }).collect();

    let mut body = serde_json::json!({
        "model": params.model,
        "max_tokens": params.max_tokens,
        "stream": false,
        "messages": messages,
    });
    if !tools.is_empty() {
        body["tools"] = serde_json::json!(tools);
        // Tell the model it can use tools.  "auto" lets the model decide;
        // use tool_choice to force a specific tool when needed.
        body["tool_choice"] = serde_json::json!("auto");
    }
    if let Some(t) = params.temperature {
        body["temperature"] = serde_json::json!(t);
    }

    // Only set temperature if explicitly configured.
    // Some models (Kimi K3) reject non-1.0 values.
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
}

/// Parse a non-streaming OpenAI Chat Completions response into TurnOutput.
async fn fold_openai_non_streaming(
    response: reqwest::Response,
    params: &RequestParams,
    on_event: &mut impl FnMut(&StreamEvent),
) -> Result<TurnOutput> {
    let body: serde_json::Value = response.json().await
        .map_err(|e| Error::Network(format!("OpenAI parse: {e}")))?;

    let model = body["model"].as_str().unwrap_or(&params.model).to_string();
    let msg_id = body["id"].as_str().unwrap_or("").to_string();
    let msg = &body["choices"][0]["message"];
    let content_text = msg["content"].as_str().unwrap_or("").to_string();

    // OpenAI returns prompt_tokens/completion_tokens, not input_tokens/output_tokens.
    let usage = serde_json::from_value::<OpenAiUsage>(body["usage"].clone())
        .unwrap_or_default();

    // Emit a synthetic usage event for the UI token counter.
    let usage_part = nonoclaw_core::UsagePart {
        input_tokens: Some(usage.prompt_tokens),
        output_tokens: Some(usage.completion_tokens),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };
    on_event(&StreamEvent::MessageStart {
        message_id: msg_id.clone(),
        model: model.clone(),
        usage: usage_part,
    });

    if !content_text.is_empty() {
        on_event(&StreamEvent::TextDelta { text: content_text.clone() });
    }

    // Parse tool_calls from the response.
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    if !content_text.is_empty() {
        content_blocks.push(ContentBlock::Text { text: content_text, cache_control: None });
    }

    let mut has_tool_calls = false;
    if let Some(tool_calls) = msg["tool_calls"].as_array() {
        for tc in tool_calls {
            let id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
            let input: serde_json::Value = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
            if !id.is_empty() && !name.is_empty() {
                on_event(&StreamEvent::ToolUseStart {
                    index: content_blocks.len(),
                    id: id.clone(),
                    name: name.clone(),
                });
                content_blocks.push(ContentBlock::ToolUse { id, name, input });
                has_tool_calls = true;
            }
        }
    }

    let stop = body["choices"][0]["finish_reason"].as_str().unwrap_or("stop");
    let stop_reason = if has_tool_calls {
        Some(nonoclaw_core::StopReason::ToolUse)
    } else {
        match stop {
            "stop" | "end_turn" => Some(nonoclaw_core::StopReason::EndTurn),
            "length" | "max_tokens" => Some(nonoclaw_core::StopReason::MaxTokens),
            "tool_calls" => Some(nonoclaw_core::StopReason::ToolUse),
            other => Some(nonoclaw_core::StopReason::Other(other.to_string())),
        }
    };

    let content = content_blocks;

    Ok(TurnOutput {
        message_id: msg_id,
        model,
        content,
        stop_reason,
        usage: nonoclaw_core::Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
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
        let preview = b.text.lines().take(8).collect::<Vec<_>>().join("\n");
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
            nonoclaw_core::MessageContent::Blocks(bs) => {
                bs.iter()
                    .map(|b| match b {
                        nonoclaw_core::ContentBlock::Text { text, .. } => text.chars().count(),
                        nonoclaw_core::ContentBlock::Image { .. } => 1200,
                        _ => 200,
                    })
                    .sum()
            }
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
        roles = params.messages.iter().map(|m| match m.role {
            nonoclaw_core::Role::User => "U",
            nonoclaw_core::Role::Assistant => "A",
        }).collect::<Vec<_>>().join(" "),
    );
}

fn dump_prompt_openai(params: &RequestParams, _body: &str) {
    let sep = "═".repeat(72);
    let thin = "─".repeat(72);

    let sys_chars: usize = params.system.iter().map(|b| b.text.chars().count()).sum();
    // Per-block breakdown for debugging large system prompts.
    let block_sizes: Vec<String> = params.system.iter().enumerate().map(|(i, b)| {
        let cached = if b.cache_control.is_some() { "[CACHED]" } else { "[UNCACHED]" };
        let lines = b.text.lines().count();
        format!("Block#{} {} {}chars {}lines", i+1, cached, b.text.chars().count(), lines)
    }).collect();

    let msg_chars: usize = params.messages.iter().map(|m| match &m.content {
        nonoclaw_core::MessageContent::Text(s) => s.chars().count(),
        nonoclaw_core::MessageContent::Blocks(bs) => {
            bs.iter().map(|b| match b {
                nonoclaw_core::ContentBlock::Text { text, .. } => text.chars().count(),
                nonoclaw_core::ContentBlock::Image { .. } => 1200,
                _ => 200,
            }).sum()
        }
    }).sum();
    let est_tokens = (sys_chars + msg_chars) / 4 + params.messages.len() * 4;

    let img_count: usize = params.messages.iter().map(|m| match &m.content {
        nonoclaw_core::MessageContent::Blocks(bs) => bs.iter().filter(|b| matches!(b, nonoclaw_core::ContentBlock::Image { .. })).count(),
        _ => 0,
    }).sum();

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
        roles = params.messages.iter().map(|m| match m.role {
            nonoclaw_core::Role::User => "U",
            nonoclaw_core::Role::Assistant => "A",
        }).collect::<Vec<_>>().join(" "),
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
}
