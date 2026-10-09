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
//! * No tool calling — the Kiro backend does not expose tool_use blocks.
//! * No streaming — the non-streaming path returns the full response in
//!   one JSON body.  We simulate streaming by emitting a single
//!   `TextDelta` after the request completes.
//! * No prompt caching, thinking, or image blocks in history.
//! * System prompts are prepended to the first user message.

use nonoclaw_core::{ContentBlock, Message, MessageContent, Role};

use crate::client::{RequestParams, StreamEvent, SystemBlock};

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
    // preceding history.
    let mut history: Vec<serde_json::Value> = Vec::new();
    let mut current_content = String::new();

    for msg in &params.messages {
        match msg.role {
            Role::User => {
                let text = extract_text(&msg.content);
                if !text.is_empty() {
                    if current_content.is_empty() && history.is_empty() {
                        // First user message — may become currentMessage.
                        current_content = text;
                    } else {
                        // Push previous current into history.
                        if !current_content.is_empty() {
                            history.push(serde_json::json!({
                                "userInputMessage": { "content": std::mem::take(&mut current_content) }
                            }));
                        }
                        current_content = text;
                    }
                }
            }
            Role::Assistant => {
                // Flush pending user message into history first.
                if !current_content.is_empty() {
                    history.push(serde_json::json!({
                        "userInputMessage": { "content": std::mem::take(&mut current_content) }
                    }));
                }
                let text = extract_text(&msg.content);
                if !text.is_empty() {
                    history.push(serde_json::json!({
                        "assistantResponseMessage": { "content": text }
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
    if let Some(mid) = model_id {
        user_input_msg["modelId"] = serde_json::Value::String(mid);
    }

    let mut conversation_state = serde_json::json!({
        "currentMessage": { "userInputMessage": user_input_msg },
        "chatTriggerType": "MANUAL"
    });
    if !history.is_empty() {
        conversation_state["history"] = serde_json::Value::Array(history);
    }

    let body = serde_json::json!({ "conversationState": conversation_state });
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
        assert_eq!(
            parsed["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "claude-sonnet-4"
        );
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
}
