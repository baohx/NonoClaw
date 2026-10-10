//! Kiro (AWS CodeWhisperer) message serialization and response parsing.
//!
//! Converts Anthropic-format `RequestParams` into the Kiro
//! `generateAssistantResponse` wire format, and parses the JSON response
//! back into `StreamEvent`s that the existing `fold_*_stream` consumers
//! understand.
//!
//! ## Kiro wire format (from reverse-engineering Kiro IDE 1.1.70)
//!
//! ```json
//! POST https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse
//! Headers: Authorization: Bearer {access_token}
//! {
//!   "conversationState": {
//!     "currentMessage": {
//!       "userInputMessage": {
//!         "content": "…",
//!         "origin": "AI_EDITOR"
//!       }
//!     },
//!     "history": [
//!       {"userInputMessage": {"content": "…"}},
//!       {"assistantResponseMessage": {"content": "…"}}
//!     ],
//!     "chatTriggerType": "MANUAL"
//!   }
//! }
//! ```
//!
//! ## Limitations (vs. native Anthropic Messages)
//!
//! * No streaming — the non-streaming path returns the full response in
//!   one JSON body.  We simulate streaming by emitting a single
//!   `TextDelta` after the request completes.
//! * No prompt caching, thinking, or image blocks in history.
//! * System prompts are prepended to the first user message.
//! * Tool calling is supported: definitions ride on the current message as
//!   `userInputMessageContext.tools`, prior calls/results replay through
//!   `assistantResponseMessage.toolUses` and
//!   `userInputMessageContext.toolResults`, and response `toolUseEvent`
//!   frames are folded into `ContentBlock::ToolUse` blocks.

use nonoclaw_core::{ContentBlock, Message, MessageContent, Role, ToolResultContent};

use crate::client::{RequestParams, StreamEvent, SystemBlock, ToolSchema};

/// Kiro API endpoint.
pub const KIRO_API_URL: &str =
    "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse";

// ── Request serialization ────────────────────────────────────────────────────

/// Convert an Anthropic-format `RequestParams` into the Kiro JSON body.
pub fn serialize_body_kiro(params: &RequestParams) -> Result<String, serde_json::Error> {
    let mut system_text = String::new();

    for block in &params.system {
        if !system_text.is_empty() {
            system_text.push_str("\n\n");
        }
        system_text.push_str(&block.text);
    }

    // Separate the last user message (becomes `currentMessage`) from the
    // preceding history.  Tool results accumulate in `pending_tool_results`
    // and are flushed as dedicated history entries only when a later
    // message arrives — the final pending set rides on `currentMessage`.
    let mut history: Vec<serde_json::Value> = Vec::new();
    let mut current_content = String::new();
    let mut pending_tool_results: Vec<serde_json::Value> = Vec::new();

    for msg in &params.messages {
        match msg.role {
            Role::User => {
                let text = extract_user_text(&msg.content);
                // Tool results ride on user messages as a context list.
                for block in iter_blocks(&msg.content) {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = block
                    {
                        pending_tool_results.push(serde_json::json!({
                            "toolUseId": tool_use_id,
                            "content": [{ "text": tool_result_text(content) }],
                            "status": if is_error.unwrap_or(false) { "error" } else { "success" },
                        }));
                    }
                }
                if !text.is_empty() {
                    // A new real user turn: flush any accumulated state into
                    // history, then this text becomes the current message.
                    flush_pending(history.as_mut(), &mut current_content, &mut pending_tool_results);
                    current_content = text;
                }
            }
            Role::Assistant => {
                // Assistant turns always flush pending user state first.
                flush_pending(history.as_mut(), &mut current_content, &mut pending_tool_results);
                let text = extract_text(&msg.content);
                let tool_uses: Vec<serde_json::Value> = iter_blocks(&msg.content)
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input, .. } => Some(
                            serde_json::json!({
                                "toolUseId": id,
                                "name": name,
                                // Wire-verified: input must be a JSON object.
                                // Stringified JSON is rejected with HTTP 500.
                                "input": input,
                            }),
                        ),
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() || !tool_uses.is_empty() {
                    let mut asm = serde_json::Map::new();
                    // Wire-verified: assistantResponseMessage must always carry a
                    // `content` field, even when the turn is tool-uses-only —
                    // omitting it is rejected with 400 REQUEST_BODY_INVALID.
                    asm.insert("content".into(), serde_json::Value::String(text));
                    if !tool_uses.is_empty() {
                        asm.insert("toolUses".into(), serde_json::Value::Array(tool_uses));
                    }
                    history.push(serde_json::json!({
                        "assistantResponseMessage": asm
                    }));
                }
            }
        }
    }

    // If there is no user message at all, treat system text as the message.
    if current_content.is_empty() && !system_text.is_empty() {
        current_content = std::mem::take(&mut system_text);
    }

    // Prepend system text to the current message (Kiro has no system field).
    if !system_text.is_empty() && !current_content.is_empty() {
        current_content = format!("{system_text}\n\n{current_content}");
    }

    // modelId mapping: "auto-kiro" → omit modelId (let Kiro route).
    let model_id = if params.model.eq_ignore_ascii_case("auto-kiro") {
        None
    } else {
        Some(model_name_to_kiro(&params.model))
    };

    let mut user_input_msg = serde_json::json!({
        "content": current_content,
        "origin": "AI_EDITOR"
    });
    // Tools (definitions) and pending tool results ride on the current
    // message inside `userInputMessageContext`.
    if !params.tools.is_empty() || !pending_tool_results.is_empty() {
        let mut ctx = serde_json::Map::new();
        if !params.tools.is_empty() {
            ctx.insert(
                "tools".into(),
                serde_json::Value::Array(
                    params.tools.iter().map(tool_schema_to_kiro).collect(),
                ),
            );
        }
        if !pending_tool_results.is_empty() {
            ctx.insert(
                "toolResults".into(),
                serde_json::Value::Array(pending_tool_results),
            );
        }
        let _ = user_input_msg
            .as_object_mut()
            .map(|m| m.insert("userInputMessageContext".into(), serde_json::Value::Object(ctx)));
    }

    let mut conversation_state = serde_json::json!({
        "currentMessage": { "userInputMessage": user_input_msg },
        "chatTriggerType": "MANUAL"
    });
    if !history.is_empty() {
        conversation_state["history"] = serde_json::Value::Array(history);
    }

    let mut body = serde_json::json!({ "conversationState": conversation_state });
    // modelId goes at the top level, NOT inside userInputMessage.
    if let Some(mid) = model_id {
        body["modelId"] = serde_json::Value::String(mid);
    }
    serde_json::to_string(&body)
}

/// Map an Anthropic model name to a Kiro modelId.
///
/// Kiro uses shortened model IDs compared to the full Anthropic date-stamped
/// versions.  We strip the 8-digit date suffix when present (e.g. 20250514).
fn model_name_to_kiro(name: &str) -> String {
    // e.g. "claude-sonnet-4-20250514" → "claude-sonnet-4"
    //      "claude-opus-4-1-20250805" → "claude-opus-4-1"
    //      "claude-haiku-3-5-20241022" → "claude-haiku-3-5"
    //      "claude-sonnet-4" → "claude-sonnet-4" (no date to strip)
    //
    // Only strip 8-digit date-like segments (YYYYMMDD format).
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() > 1
        && parts
            .last()
            .map_or(false, |s| s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()))
    {
        parts[..parts.len() - 1].join("-")
    } else {
        name.to_string()
    }
}

/// Convert an Anthropic `ToolSchema` into Kiro's `toolSpecification` shape:
/// `{ toolSpecification: { name, description, inputSchema: { json } } }`.
fn tool_schema_to_kiro(tool: &ToolSchema) -> serde_json::Value {
    let description = if tool.description.trim().is_empty() {
        tool.name.trim().to_string()
    } else {
        tool.description.clone()
    };
    // Kiro chokes on empty/absent schemas — normalise to an empty object schema.
    let schema = if tool.input_schema.is_null() {
        serde_json::json!({ "type": "object", "properties": {} })
    } else {
        tool.input_schema.clone()
    };
    serde_json::json!({
        "toolSpecification": {
            "name": tool.name,
            "description": description,
            "inputSchema": { "json": schema },
        }
    })
}

/// Iterate over the content blocks of a message (empty when the content is
/// plain text).
fn iter_blocks(content: &MessageContent) -> std::slice::Iter<'_, ContentBlock> {
    static EMPTY: std::sync::OnceLock<Vec<ContentBlock>> = std::sync::OnceLock::new();
    match content {
        MessageContent::Text(_) => EMPTY.get_or_init(Vec::new).iter(),
        MessageContent::Blocks(blocks) => blocks.iter(),
    }
}

/// Push any accumulated (pending) user text and tool results into the
/// conversation history.  Called when a later turn arrives; whatever
/// remains pending at the end of the loop rides on `currentMessage`.
fn flush_pending(
    history: &mut Vec<serde_json::Value>,
    current_content: &mut String,
    pending_tool_results: &mut Vec<serde_json::Value>,
) {
    if !current_content.is_empty() {
        history.push(serde_json::json!({
            "userInputMessage": { "content": std::mem::take(current_content) }
        }));
    }
    if !pending_tool_results.is_empty() {
        history.push(serde_json::json!({
            "userInputMessage": {
                "content": "",
                "origin": "AI_EDITOR",
                "userInputMessageContext": {
                    "toolResults": std::mem::take(pending_tool_results)
                }
            }
        }));
    }
}

/// Extract only the user-authored text of a message — tool results are
/// excluded because they are replayed through `userInputMessageContext`.
fn extract_user_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                if let ContentBlock::Text { text, .. } = block {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            out
        }
    }
}

/// Flatten a tool result's content into the single text string Kiro expects.
fn tool_result_text(content: &ToolResultContent) -> String {
    match content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(blocks) => {
            let mut out = String::new();
            for b in blocks {
                if let ContentBlock::Text { text, .. } = b {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            out
        }
    }
}

/// Extract plain text from `MessageContent`.
fn extract_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        // Flatten tool results into text.
                        match content {
                            nonoclaw_core::ToolResultContent::Text(t) => {
                                if !out.is_empty() {
                                    out.push('\n');
                                }
                                out.push_str(t);
                            }
                            nonoclaw_core::ToolResultContent::Blocks(blocks) => {
                                for b in blocks {
                                    if let ContentBlock::Text { text, .. } = b {
                                        if !out.is_empty() {
                                            out.push('\n');
                                        }
                                        out.push_str(text);
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        // Images, thinking, tool_use: skip in v1.
                    }
                }
            }
            out
        }
    }
}

// ── AWS Event Stream parsing ─────────────────────────────────────────────────

/// One parsed event-stream frame.
#[derive(Debug)]
pub struct KiroEventFrame {
    pub event_type: String,
    pub payload: serde_json::Value,
}

/// Parse the raw AWS event-stream body returned by `generateAssistantResponse`.
///
/// Wire layout (big-endian):
/// ```text
/// [4B total_len] [4B headers_len] [4B prelude_crc] [headers…] [payload] [4B msg_crc]
/// ```
/// `headers_len` covers the 4-byte prelude CRC + header entries.
/// Each header entry: `[1B name_len][name][1B val_type][value]`
/// For val_type 7 (string): `[2B big-endian len][UTF-8 bytes]`.
pub fn parse_aws_event_stream(raw: &[u8]) -> Vec<KiroEventFrame> {
    let mut frames = Vec::new();
    let mut offset = 0usize;

    while offset + 16 <= raw.len() {
        let total_len = u32::from_be_bytes([raw[offset], raw[offset + 1], raw[offset + 2], raw[offset + 3]]) as usize;
        let headers_len = u32::from_be_bytes([raw[offset + 4], raw[offset + 5], raw[offset + 6], raw[offset + 7]]) as usize;

        if total_len < 16 || offset + total_len > raw.len() {
            break;
        }

        // Correct layout (headers_len does NOT include the 4-byte prelude CRC):
        //   [0..4]   total_len
        //   [4..8]   headers_len
        //   [8..12]  prelude_crc
        //   [12 .. 12+headers_len]  header entries
        //   [12+headers_len .. total_len-4]  payload
        //   [total_len-4 .. total_len]  msg_crc
        let hdr_start = offset + 12;
        let hdr_end = hdr_start + headers_len;
        let payload_start = hdr_end;
        let payload_end = offset + total_len - 4; // trailing msg CRC

        // Parse headers to find :event-type.
        let hdr_bytes = &raw[hdr_start..hdr_end.min(raw.len())];
        let mut event_type = String::new();
        let mut i = 0usize;
        while i < hdr_bytes.len() {
            let name_len = hdr_bytes[i] as usize;
            i += 1;
            if i + name_len > hdr_bytes.len() { break; }
            let name = match std::str::from_utf8(&hdr_bytes[i..i + name_len]) {
                Ok(n) => n,
                Err(_) => break,
            };
            i += name_len;
            if i >= hdr_bytes.len() { break; }
            let val_type = hdr_bytes[i];
            i += 1;
            if val_type == 7 {
                if i + 2 > hdr_bytes.len() { break; }
                let vlen = u16::from_be_bytes([hdr_bytes[i], hdr_bytes[i + 1]]) as usize;
                i += 2;
                if i + vlen > hdr_bytes.len() { break; }
                let val = String::from_utf8_lossy(&hdr_bytes[i..i + vlen]).into_owned();
                i += vlen;
                if name == ":event-type" {
                    event_type = val;
                }
            } else {
                break;
            }
        }

        // Parse payload JSON.
        if payload_start < payload_end && payload_end <= raw.len() {
            if let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&raw[payload_start..payload_end]) {
                frames.push(KiroEventFrame { event_type, payload });
            }
        }

        offset += total_len;
    }

    frames
}

/// Extract the assistant text from a list of event-stream frames.
///
/// Concatenates the `content` field of every `assistantResponseEvent` frame.
pub fn extract_text_from_frames(frames: &[KiroEventFrame]) -> String {
    let mut text = String::new();
    for frame in frames {
        if frame.event_type == "assistantResponseEvent" {
            if let Some(content) = frame.payload.get("content").and_then(|v| v.as_str()) {
                text.push_str(content);
            }
        }
    }
    text
}

/// Extract the metering usage from event-stream frames.
pub fn extract_metering_from_frames(frames: &[KiroEventFrame]) -> Option<f64> {
    for frame in frames {
        if frame.event_type == "meteringEvent" {
            return frame.payload.get("usage").and_then(|v| v.as_f64());
        }
    }
    None
}

/// Extract the context usage percentage from event-stream frames.
pub fn extract_context_usage_from_frames(frames: &[KiroEventFrame]) -> Option<f64> {
    for frame in frames {
        if frame.event_type == "contextUsageEvent" {
            return frame.payload.get("contextUsagePercentage").and_then(|v| v.as_f64());
        }
    }
    None
}

/// A tool call reconstructed from response `toolUseEvent` frames.
///
/// Kiro streams each tool call as a sequence of frames sharing a
/// `toolUseId`: `name` on the first frame, `input` string deltas
/// throughout, and `stop: true` on the closing frame.
#[derive(Debug, Clone, PartialEq)]
pub struct KiroToolUse {
    pub tool_use_id: String,
    pub name: String,
    /// Parsed input object (empty object when the accumulated JSON is empty).
    pub input: serde_json::Value,
}

/// Extract tool calls from a list of event-stream frames.
///
/// Frames are grouped by `toolUseId` preserving first-seen order; `input`
/// fragments are concatenated then parsed as JSON.
pub fn extract_tool_uses_from_frames(frames: &[KiroEventFrame]) -> Vec<KiroToolUse> {
    let mut order: Vec<String> = Vec::new();
    let mut acc: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new(); // id → (name, input buffer)

    for frame in frames {
        if frame.event_type != "toolUseEvent" {
            continue;
        }
        let id = frame
            .payload
            .get("toolUseId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let entry = acc.entry(id.clone()).or_insert_with(|| {
            order.push(id);
            (String::new(), String::new())
        });
        if let Some(name) = frame.payload.get("name").and_then(|v| v.as_str()) {
            if entry.0.is_empty() {
                entry.0 = name.to_string();
            }
        }
        if let Some(chunk) = frame.payload.get("input").and_then(|v| v.as_str()) {
            entry.1.push_str(chunk);
        }
    }

    order
        .into_iter()
        .map(|id| {
            let (name, input_str) = acc.remove(&id).unwrap_or_default();
            let input = if input_str.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&input_str).unwrap_or_else(|_| serde_json::json!({}))
            };
            KiroToolUse { tool_use_id: id, name, input }
        })
        .collect()
}

// ── Response parsing ─────────────────────────────────────────────────────────

/// Parsed Kiro `generateAssistantResponse` response body.
#[derive(Debug, serde::Deserialize)]
pub struct KiroGenerateResponse {
    #[serde(rename = "generateAssistantResponseResponse")]
    pub response: Option<KiroAssistantResponse>,
    /// Conversation ID to reuse for multi-turn context.
    #[serde(rename = "conversationId")]
    pub conversation_id: Option<String>,
    /// HTTP request ID (for debugging).
    #[serde(rename = "$metadata")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
pub struct KiroAssistantResponse {
    pub content: Option<String>,
    #[serde(rename = "messageId")]
    pub message_id: Option<String>,
    #[serde(rename = "modelId")]
    pub model_id: Option<String>,
}

/// Extract the assistant text from a Kiro response.
pub fn kiro_response_text(resp: &KiroGenerateResponse) -> String {
    resp.response
        .as_ref()
        .and_then(|r| r.content.clone())
        .unwrap_or_default()
}

/// Extract the model ID from a Kiro response (falls back to empty).
pub fn kiro_response_model(resp: &KiroGenerateResponse, requested: &str) -> String {
    resp.response
        .as_ref()
        .and_then(|r| r.model_id.clone())
        .unwrap_or_else(|| requested.to_string())
}

/// Extract the message ID from a Kiro response.
pub fn kiro_response_message_id(resp: &KiroGenerateResponse) -> String {
    resp.response
        .as_ref()
        .and_then(|r| r.message_id.clone())
        .unwrap_or_default()
}

// ── Simulated stream events ─────────────────────────────────────────────────

/// Emit a minimal set of `StreamEvent`s that represent a completed non-streaming
/// Kiro response.  This lets the existing `TurnAccumulator` and event consumers
/// work without modification.
pub fn emit_kiro_stream_events(
    params: &RequestParams,
    resp: &KiroGenerateResponse,
) -> Vec<StreamEvent> {
    let text = kiro_response_text(resp);
    let model = kiro_response_model(resp, &params.model);
    let message_id = kiro_response_message_id(resp);

    vec![
        StreamEvent::MessageStart {
            message_id: message_id.clone(),
            model: model.clone(),
            usage: nonoclaw_core::UsagePart {
                input_tokens: Some(0),
                output_tokens: Some(0),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        },
        StreamEvent::TextDelta { text },
        StreamEvent::MessageStop,
    ]
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_core::{Message, MessageContent, Role};

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_string()),
            ts: None,
        }
    }

    #[test]
    fn model_name_mapping_strips_date() {
        assert_eq!(
            model_name_to_kiro("claude-sonnet-4-20250514"),
            "claude-sonnet-4"
        );
        assert_eq!(
            model_name_to_kiro("claude-opus-4-1-20250805"),
            "claude-opus-4-1"
        );
        assert_eq!(
            model_name_to_kiro("claude-haiku-3-5-20241022"),
            "claude-haiku-3-5"
        );
        assert_eq!(model_name_to_kiro("claude-sonnet-4"), "claude-sonnet-4");
        assert_eq!(model_name_to_kiro("auto-kiro"), "auto-kiro");
    }

    #[test]
    fn serialize_simple_message() {
        let params = RequestParams {
            model: "auto-kiro".into(),
            messages: vec![text_msg(Role::User, "Hello")],
            system: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 4096,
            thinking: None,
            temperature: None,
            betas: vec![],
            extra_body: None,
            session_id: None,
            trace_label: None,
        };
        let body = serialize_body_kiro(&params).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();

        // auto-kiro → no modelId field
        assert!(parsed["conversationState"]["currentMessage"]["userInputMessage"]
            .get("modelId")
            .is_none());
        assert_eq!(
            parsed["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "Hello"
        );
        assert_eq!(
            parsed["conversationState"]["chatTriggerType"],
            "MANUAL"
        );
    }

    #[test]
    fn serialize_with_model() {
        let params = RequestParams {
            model: "claude-sonnet-4-20250514".into(),
            messages: vec![text_msg(Role::User, "Hi")],
            system: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 4096,
            thinking: None,
            temperature: None,
            betas: vec![],
            extra_body: None,
            session_id: None,
            trace_label: None,
        };
        let body = serialize_body_kiro(&params).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        // modelId goes at the top level, NOT inside userInputMessage.
        assert_eq!(parsed["modelId"], "claude-sonnet-4");
    }

    #[test]
    fn serialize_with_history() {
        let params = RequestParams {
            model: "auto-kiro".into(),
            messages: vec![
                text_msg(Role::User, "First"),
                text_msg(Role::Assistant, "Reply"),
                text_msg(Role::User, "Second"),
            ],
            system: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 4096,
            thinking: None,
            temperature: None,
            betas: vec![],
            extra_body: None,
            session_id: None,
            trace_label: None,
        };
        let body = serialize_body_kiro(&params).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();

        let history = parsed["conversationState"]["history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert!(history[0].get("userInputMessage").is_some());
        assert!(history[1].get("assistantResponseMessage").is_some());
        assert_eq!(
            parsed["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "Second"
        );
    }

    #[test]
    fn serialize_system_prepended() {
        let params = RequestParams {
            model: "auto-kiro".into(),
            messages: vec![text_msg(Role::User, "Hello")],
            system: vec![SystemBlock {
                kind: "text".into(),
                text: "You are helpful.".into(),
                cache_control: None,
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: 4096,
            thinking: None,
            temperature: None,
            betas: vec![],
            extra_body: None,
            session_id: None,
            trace_label: None,
        };
        let body = serialize_body_kiro(&params).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "You are helpful.\n\nHello"
        );
    }

    #[test]
    fn emit_stream_events_structure() {
        let params = RequestParams {
            model: "auto-kiro".into(),
            messages: vec![text_msg(Role::User, "Hi")],
            system: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 4096,
            thinking: None,
            temperature: None,
            betas: vec![],
            extra_body: None,
            session_id: None,
            trace_label: None,
        };
        let resp: KiroGenerateResponse = serde_json::from_str(
            r#"{
                "generateAssistantResponseResponse": {
                    "content": "Hello there!",
                    "messageId": "msg-123",
                    "modelId": "claude-sonnet-4"
                }
            }"#,
        )
        .unwrap();

        let events = emit_kiro_stream_events(&params, &resp);
        assert_eq!(events.len(), 3); // MessageStart, TextDelta, MessageStop

        match &events[0] {
            StreamEvent::MessageStart {
                message_id,
                model,
                ..
            } => {
                assert_eq!(message_id, "msg-123");
                assert_eq!(model, "claude-sonnet-4");
            }
            _ => panic!("expected MessageStart"),
        }

        match &events[1] {
            StreamEvent::TextDelta { text } => assert_eq!(text, "Hello there!"),
            _ => panic!("expected TextDelta"),
        }

        match &events[2] {
            StreamEvent::MessageStop => {}
            _ => panic!("expected MessageStop"),
        }
    }

    // ── AWS Event Stream parser tests ──────────────────────────────────────

    /// Build a single AWS event-stream frame from an event type and JSON payload.
    fn build_event_frame(event_type: &str, payload_json: &str) -> Vec<u8> {
        let payload = payload_json.as_bytes();

        // Build headers: :event-type, :content-type, :message-type
        let mut headers = Vec::new();
        for (name, val) in [
            (":event-type", event_type),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7u8); // string type
            headers.extend_from_slice(&(val.len() as u16).to_be_bytes());
            headers.extend_from_slice(val.as_bytes());
        }

        let headers_len = headers.len(); // does NOT include prelude CRC
        let total_len = 8 + 4 + headers_len + payload.len() + 4; // prelude + prelude_crc + headers + payload + msg_crc

        let mut frame = Vec::new();
        frame.extend_from_slice(&(total_len as u32).to_be_bytes());
        frame.extend_from_slice(&(headers_len as u32).to_be_bytes());
        frame.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]); // prelude CRC (not validated)
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&[0xca, 0xfe, 0xba, 0xbe]); // msg CRC (not validated)
        frame
    }

    #[test]
    fn parse_event_stream_frames() {
        let mut raw = Vec::new();
        raw.extend(build_event_frame("assistantResponseEvent", r#"{"content":"Hello"}"#));
        raw.extend(build_event_frame("assistantResponseEvent", r#"{"content":" world"}"#));
        raw.extend(build_event_frame("contextUsageEvent", r#"{"contextUsagePercentage":0.5}"#));
        raw.extend(build_event_frame(
            "meteringEvent",
            r#"{"unit":"credit","unitPlural":"credits","usage":0.01}"#,
        ));

        let frames = parse_aws_event_stream(&raw);
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[0].event_type, "assistantResponseEvent");
        assert_eq!(frames[1].event_type, "assistantResponseEvent");
        assert_eq!(frames[2].event_type, "contextUsageEvent");
        assert_eq!(frames[3].event_type, "meteringEvent");
    }

    #[test]
    fn extract_text_from_event_frames() {
        let mut raw = Vec::new();
        raw.extend(build_event_frame("assistantResponseEvent", r#"{"content":"Hello"}"#));
        raw.extend(build_event_frame("assistantResponseEvent", r#"{"content":" there"}"#));
        raw.extend(build_event_frame("meteringEvent", r#"{"usage":0.01}"#));

        let frames = parse_aws_event_stream(&raw);
        let text = extract_text_from_frames(&frames);
        assert_eq!(text, "Hello there");
    }

    #[test]
    fn extract_metering_and_context_usage() {
        let mut raw = Vec::new();
        raw.extend(build_event_frame("assistantResponseEvent", r#"{"content":"Hi"}"#));
        raw.extend(build_event_frame("contextUsageEvent", r#"{"contextUsagePercentage":0.75}"#));
        raw.extend(build_event_frame(
            "meteringEvent",
            r#"{"unit":"credit","unitPlural":"credits","usage":0.025}"#,
        ));

        let frames = parse_aws_event_stream(&raw);
        assert_eq!(extract_metering_from_frames(&frames), Some(0.025));
        assert_eq!(extract_context_usage_from_frames(&frames), Some(0.75));
    }

    #[test]
    fn parse_empty_event_stream() {
        let frames = parse_aws_event_stream(&[]);
        assert!(frames.is_empty());
    }

    /// Real AWS event-stream response captured from a live Kiro API call.
    /// Contains 4 frames: 2× assistantResponseEvent, 1× contextUsageEvent,
    /// 1× meteringEvent.  Text: "Hello there friend".
    const REAL_KIRO_RESPONSE: &[u8] = &[
        0x00, 0x00, 0x00, 0x85, 0x00, 0x00, 0x00, 0x5c, 0x7e, 0xf9, 0xfd, 0x54, 0x0b, 0x3a, 0x65, 0x76,
        0x65, 0x6e, 0x74, 0x2d, 0x74, 0x79, 0x70, 0x65, 0x07, 0x00, 0x16, 0x61, 0x73, 0x73, 0x69, 0x73,
        0x74, 0x61, 0x6e, 0x74, 0x52, 0x65, 0x73, 0x70, 0x6f, 0x6e, 0x73, 0x65, 0x45, 0x76, 0x65, 0x6e,
        0x74, 0x0d, 0x3a, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d, 0x74, 0x79, 0x70, 0x65, 0x07,
        0x00, 0x10, 0x61, 0x70, 0x70, 0x6c, 0x69, 0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x2f, 0x6a, 0x73,
        0x6f, 0x6e, 0x0d, 0x3a, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x2d, 0x74, 0x79, 0x70, 0x65,
        0x07, 0x00, 0x05, 0x65, 0x76, 0x65, 0x6e, 0x74, 0x7b, 0x22, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e,
        0x74, 0x22, 0x3a, 0x22, 0x48, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x74, 0x68, 0x65, 0x72, 0x65, 0x22,
        0x7d, 0x54, 0x78, 0xeb, 0x4a, 0x00, 0x00, 0x00, 0x81, 0x00, 0x00, 0x00, 0x5c, 0x8b, 0x79, 0x5b,
        0x94, 0x0b, 0x3a, 0x65, 0x76, 0x65, 0x6e, 0x74, 0x2d, 0x74, 0x79, 0x70, 0x65, 0x07, 0x00, 0x16,
        0x61, 0x73, 0x73, 0x69, 0x73, 0x74, 0x61, 0x6e, 0x74, 0x52, 0x65, 0x73, 0x70, 0x6f, 0x6e, 0x73,
        0x65, 0x45, 0x76, 0x65, 0x6e, 0x74, 0x0d, 0x3a, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d,
        0x74, 0x79, 0x70, 0x65, 0x07, 0x00, 0x10, 0x61, 0x70, 0x70, 0x6c, 0x69, 0x63, 0x61, 0x74, 0x69,
        0x6f, 0x6e, 0x2f, 0x6a, 0x73, 0x6f, 0x6e, 0x0d, 0x3a, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65,
        0x2d, 0x74, 0x79, 0x70, 0x65, 0x07, 0x00, 0x05, 0x65, 0x76, 0x65, 0x6e, 0x74, 0x7b, 0x22, 0x63,
        0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x22, 0x3a, 0x22, 0x20, 0x66, 0x72, 0x69, 0x65, 0x6e, 0x64,
        0x22, 0x7d, 0x38, 0xf6, 0x28, 0xad, 0x00, 0x00, 0x00, 0x94, 0x00, 0x00, 0x00, 0x57, 0xb4, 0xab,
        0x9a, 0xee, 0x0b, 0x3a, 0x65, 0x76, 0x65, 0x6e, 0x74, 0x2d, 0x74, 0x79, 0x70, 0x65, 0x07, 0x00,
        0x11, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x78, 0x74, 0x55, 0x73, 0x61, 0x67, 0x65, 0x45, 0x76, 0x65,
        0x6e, 0x74, 0x0d, 0x3a, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d, 0x74, 0x79, 0x70, 0x65,
        0x07, 0x00, 0x10, 0x61, 0x70, 0x70, 0x6c, 0x69, 0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x2f, 0x6a,
        0x73, 0x6f, 0x6e, 0x0d, 0x3a, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x2d, 0x74, 0x79, 0x70,
        0x65, 0x07, 0x00, 0x05, 0x65, 0x76, 0x65, 0x6e, 0x74, 0x7b, 0x22, 0x63, 0x6f, 0x6e, 0x74, 0x65,
        0x78, 0x74, 0x55, 0x73, 0x61, 0x67, 0x65, 0x50, 0x65, 0x72, 0x63, 0x65, 0x6e, 0x74, 0x61, 0x67,
        0x65, 0x22, 0x3a, 0x30, 0x2e, 0x35, 0x38, 0x38, 0x34, 0x30, 0x30, 0x30, 0x30, 0x36, 0x32, 0x39,
        0x34, 0x32, 0x35, 0x30, 0x35, 0x7d, 0xe0, 0x8e, 0x9a, 0x67, 0x00, 0x00, 0x00, 0xa8, 0x00, 0x00,
        0x00, 0x53, 0xd7, 0x17, 0x0b, 0x70, 0x0b, 0x3a, 0x65, 0x76, 0x65, 0x6e, 0x74, 0x2d, 0x74, 0x79,
        0x70, 0x65, 0x07, 0x00, 0x0d, 0x6d, 0x65, 0x74, 0x65, 0x72, 0x69, 0x6e, 0x67, 0x45, 0x76, 0x65,
        0x6e, 0x74, 0x0d, 0x3a, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d, 0x74, 0x79, 0x70, 0x65,
        0x07, 0x00, 0x10, 0x61, 0x70, 0x70, 0x6c, 0x69, 0x63, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x2f, 0x6a,
        0x73, 0x6f, 0x6e, 0x0d, 0x3a, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x2d, 0x74, 0x79, 0x70,
        0x65, 0x07, 0x00, 0x05, 0x65, 0x76, 0x65, 0x6e, 0x74, 0x7b, 0x22, 0x75, 0x6e, 0x69, 0x74, 0x22,
        0x3a, 0x22, 0x63, 0x72, 0x65, 0x64, 0x69, 0x74, 0x22, 0x2c, 0x22, 0x75, 0x6e, 0x69, 0x74, 0x50,
        0x6c, 0x75, 0x72, 0x61, 0x6c, 0x22, 0x3a, 0x22, 0x63, 0x72, 0x65, 0x64, 0x69, 0x74, 0x73, 0x22,
        0x2c, 0x22, 0x75, 0x73, 0x61, 0x67, 0x65, 0x22, 0x3a, 0x30, 0x2e, 0x30, 0x31, 0x30, 0x36, 0x35,
        0x38, 0x33, 0x30, 0x34, 0x34, 0x37, 0x37, 0x36, 0x31, 0x31, 0x39, 0x34, 0x32, 0x7d, 0x83, 0xec,
        0x99, 0xa2,
    ];

    #[test]
    fn parse_real_kiro_response() {
        let frames = parse_aws_event_stream(REAL_KIRO_RESPONSE);
        assert_eq!(frames.len(), 4, "expected 4 event frames");

        // Frame 0 & 1: assistantResponseEvent
        assert_eq!(frames[0].event_type, "assistantResponseEvent");
        assert_eq!(frames[1].event_type, "assistantResponseEvent");

        // Frame 2: contextUsageEvent
        assert_eq!(frames[2].event_type, "contextUsageEvent");

        // Frame 3: meteringEvent
        assert_eq!(frames[3].event_type, "meteringEvent");

        // Extract full text
        let text = extract_text_from_frames(&frames);
        assert_eq!(text, "Hello there friend");

        // Metering
        let usage = extract_metering_from_frames(&frames);
        assert!(usage.is_some());
        assert!((usage.unwrap() - 0.010658304477611942).abs() < 1e-15);

        // Context usage
        let ctx = extract_context_usage_from_frames(&frames);
        assert!(ctx.is_some());
        assert!((ctx.unwrap() - 0.5884000062942505).abs() < 1e-15);
    }

    fn base_params(messages: Vec<Message>) -> RequestParams {
        RequestParams {
            model: "auto-kiro".into(),
            messages,
            system: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 4096,
            thinking: None,
            temperature: None,
            betas: vec![],
            extra_body: None,
            session_id: None,
            trace_label: None,
        }
    }

    #[test]
    fn serialize_tools_and_tool_history() {
        // Conversation: user asks → assistant calls a tool → user returns
        // the result → user asks follow-up (current message).
        let assistant = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text { text: "Let me check.".into(), cache_control: None },
                ContentBlock::ToolUse {
                    id: "tu_1".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"city": "Beijing"}),
                    cache_control: None,
                },
            ]),
            ts: None,
        };
        let user_result = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".into(),
                content: nonoclaw_core::ToolResultContent::Text("22C sunny".into()),
                is_error: Some(false),
                cache_control: None,
            }]),
            ts: None,
        };
        let params = RequestParams {
            tools: vec![ToolSchema {
                name: "get_weather".into(),
                description: "Get current weather".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"]
                }),
                cache_control: None,
            }],
            ..base_params(vec![text_msg(Role::User, "weather in Beijing?"), assistant, user_result, text_msg(Role::User, "thanks")])
        };

        let body = serialize_body_kiro(&params).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let conv = &parsed["conversationState"];

        // Tool definition rides on currentMessage.userInputMessageContext.tools
        let ctx = &conv["currentMessage"]["userInputMessage"]["userInputMessageContext"];
        let tools = ctx["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["toolSpecification"]["name"], "get_weather");
        assert_eq!(
            tools[0]["toolSpecification"]["inputSchema"]["json"]["type"],
            "object"
        );

        // History: user question, assistant text+toolUses, toolResults entry,
        // (follow-up became currentMessage).
        let history = conv["history"].as_array().unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0]["userInputMessage"]["content"], "weather in Beijing?");
        let asm = &history[1]["assistantResponseMessage"];
        assert_eq!(asm["content"], "Let me check.");
        let tu = &asm["toolUses"][0];
        assert_eq!(tu["toolUseId"], "tu_1");
        assert_eq!(tu["name"], "get_weather");
        // Wire-verified: input must be a JSON object, not a stringified one.
        assert_eq!(tu["input"], serde_json::json!({"city": "Beijing"}));
        let tr_entry = &history[2]["userInputMessage"]["userInputMessageContext"]["toolResults"];
        assert_eq!(tr_entry[0]["toolUseId"], "tu_1");
        assert_eq!(tr_entry[0]["content"][0]["text"], "22C sunny");
        assert_eq!(tr_entry[0]["status"], "success");
    }

    #[test]
    fn serialize_tool_only_assistant_keeps_content_field() {
        // Wire-verified (2026-10-10): assistantResponseMessage carrying only
        // toolUses and no `content` field is rejected by the Kiro backend with
        // 400 REQUEST_BODY_INVALID. `content: ""` must always be present.
        let assistant = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "tu_9".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "/tmp/x"}),
                cache_control: None,
            }]),
            ts: None,
        };
        let followup = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "tu_9".into(),
                content: nonoclaw_core::ToolResultContent::Text("data".into()),
                is_error: None,
                cache_control: None,
            }]),
            ts: None,
        };
        let params = base_params(vec![assistant, followup]);
        let body = serialize_body_kiro(&params).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let asm = &parsed["conversationState"]["history"][0]["assistantResponseMessage"];
        assert!(asm.get("content").is_some());
        assert_eq!(asm["content"], "");
        assert_eq!(asm["toolUses"][0]["toolUseId"], "tu_9");
    }

    #[test]
    fn serialize_error_tool_result_status() {
        let user_result = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "tu_e".into(),
                content: nonoclaw_core::ToolResultContent::Text("boom".into()),
                is_error: Some(true),
                cache_control: None,
            }]),
            ts: None,
        };
        let params = base_params(vec![user_result]);
        let body = serialize_body_kiro(&params).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        // No prior user text → the tool-result entry is the current message's
        // context (only tool content in conversation).
        let tr = &parsed["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["toolResults"];
        assert_eq!(tr[0]["status"], "error");
    }

    #[test]
    fn extract_tool_uses_accumulates_input_fragments() {
        let mk = |payload: serde_json::Value| KiroEventFrame {
            event_type: "toolUseEvent".into(),
            payload,
        };
        let frames = vec![
            mk(serde_json::json!({"toolUseId": "a", "name": "read_file", "input": "{\"pa"})),
            mk(serde_json::json!({"toolUseId": "a", "input": "th\":\"/tmp\"}"})),
            mk(serde_json::json!({"toolUseId": "a", "stop": true})),
            mk(serde_json::json!({"toolUseId": "b", "name": "bash", "input": "{\"cmd\":\"ls\"}"})),
            mk(serde_json::json!({"toolUseId": "b", "stop": true})),
        ];
        let uses = extract_tool_uses_from_frames(&frames);
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].tool_use_id, "a");
        assert_eq!(uses[0].name, "read_file");
        assert_eq!(uses[0].input, serde_json::json!({"path": "/tmp"}));
        assert_eq!(uses[1].name, "bash");
        assert_eq!(uses[1].input, serde_json::json!({"cmd": "ls"}));
    }

    #[test]
    fn extract_tool_uses_ignores_malformed_json() {
        let frames = vec![KiroEventFrame {
            event_type: "toolUseEvent".into(),
            payload: serde_json::json!({"toolUseId": "x", "name": "t", "input": "not-json{"}),
        }];
        let uses = extract_tool_uses_from_frames(&frames);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].input, serde_json::json!({}));
    }
}
