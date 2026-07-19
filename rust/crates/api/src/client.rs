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

pub struct Client {
    http: reqwest::Client,
    api_key: Option<String>,
    auth_token: Option<String>,
    base_url: String,
    retry: RetryConfig,
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
        })
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
    pub fn base_url(&self) -> &str { &self.base_url }
    /// The configured API key, if any.
    pub fn api_key(&self) -> Option<&str> { self.api_key.as_deref() }

    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    fn build_request(&self, params: &RequestParams) -> Result<reqwest::RequestBuilder> {
        let body = serialize_body(params)?;
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut req = self
            .http
            .post(url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        if let Some(key) = &self.api_key {
            req = req.header("x-api-key", key);
        }
        if let Some(token) = &self.auth_token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        if !params.betas.is_empty() {
            req = req.header("anthropic-beta", params.betas.join(","));
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
        fold_stream(response, &mut on_event).await
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
fn serialize_body(params: &RequestParams) -> Result<String> {
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
