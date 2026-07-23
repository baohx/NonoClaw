//! Messages and content blocks.
//!
//! Mirrors the Anthropic Messages API wire shape plus the internal message
//! taxonomy from `src/types/message.ts` (absent from this extraction —
//! reconstructed from `src/Tool.ts`, `src/query.ts`, `src/QueryEngine.ts` and
//! the streaming consumer in `src/services/api/claude.ts`).

use serde::{Deserialize, Serialize};

/// Conversation role. The API only accepts `user` and `assistant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// Prompt-cache breakpoint. Placed on a content block (or system block) to mark
/// the end of a cacheable prefix. Mirrors `cache_control` in the TS request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: CacheControlKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlKind {
    Ephemeral,
}

/// A single content block. Serializes to/from the API JSON via a `type` tag.
///
/// During streaming the API layer accumulates `tool_use` input from
/// `input_json_delta.partial_json` fragments and only constructs
/// `ContentBlock::ToolUse` with the parsed `input` once the block closes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
    },
    /// Emitted by the model when it wants to call a tool.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Emitted by us (as a `user` message) to return a tool's result.
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Model reasoning block (extended thinking). Carries a signature so the
    /// API can verify the chain across turns.
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

impl ContentBlock {
    pub fn text<S: Into<String>>(s: S) -> Self {
        ContentBlock::Text {
            text: s.into(),
            cache_control: None,
        }
    }

    pub fn tool_result<S: Into<String>>(tool_use_id: String, content: S, is_error: bool) -> Self {
        ContentBlock::ToolResult {
            tool_use_id,
            content: ToolResultContent::Text(content.into()),
            is_error: Some(is_error),
        }
    }

    /// `true` if this block represents a model tool call.
    pub fn is_tool_use(&self) -> bool {
        matches!(self, ContentBlock::ToolUse { .. })
    }
}

/// Base64 image source used for model attachments. URL retrieval remains the
/// responsibility of WebFetch rather than the message wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub kind: String, // "base64"
    pub media_type: String,
    pub data: String,
}

/// A tool result's `content` may be a plain string or an array of blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl ToolResultContent {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ToolResultContent::Text(s) => Some(s),
            ToolResultContent::Blocks(_) => None,
        }
    }
}

/// Message content: either a plain string or an array of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    pub fn from_text<S: Into<String>>(s: S) -> Self {
        MessageContent::Text(s.into())
    }
    pub fn from_blocks(blocks: Vec<ContentBlock>) -> Self {
        MessageContent::Blocks(blocks)
    }
}

/// An API-exchangeable message: a role plus content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

impl Message {
    pub fn user(content: MessageContent) -> Self {
        Message {
            role: Role::User,
            content,
        }
    }
    pub fn assistant(content: MessageContent) -> Self {
        Message {
            role: Role::Assistant,
            content,
        }
    }
    /// Extract any `ToolUse` blocks carried by this message (assistant turns).
    pub fn tool_uses(&self) -> Vec<(String, String, serde_json::Value)> {
        match &self.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Why the model stopped generating for a turn. Mirrors the API `stop_reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    Refusal,
    ModelContextWindowExceeded,
    Other(String),
}

impl StopReason {
    pub fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::PauseTurn => "pause_turn",
            StopReason::Refusal => "refusal",
            StopReason::ModelContextWindowExceeded => "model_context_window_exceeded",
            StopReason::Other(s) => s.as_str(),
        }
    }
    /// `true` when the model is yielding control to run tools (loop continues).
    pub fn wants_tools(&self) -> bool {
        matches!(self, StopReason::ToolUse)
    }
}

impl Serialize for StopReason {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Ok(match s.as_str() {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "pause_turn" => StopReason::PauseTurn,
            "refusal" => StopReason::Refusal,
            "model_context_window_exceeded" => StopReason::ModelContextWindowExceeded,
            other => StopReason::Other(other.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_block_roundtrip() {
        let b = ContentBlock::text("hi");
        let j = serde_json::to_value(&b).unwrap();
        assert_eq!(j["type"], "text");
        assert_eq!(j["text"], "hi");
        let back: ContentBlock = serde_json::from_value(j).unwrap();
        assert!(matches!(back, ContentBlock::Text { .. }));
    }

    #[test]
    fn tool_use_roundtrip() {
        let b = ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "/a"}),
        };
        let j = serde_json::to_value(&b).unwrap();
        assert_eq!(j["type"], "tool_use");
        assert_eq!(j["input"]["file_path"], "/a");
    }

    #[test]
    fn tool_result_content_untagged() {
        let s = r#"{"type":"tool_result","tool_use_id":"x","content":"done"}"#;
        let b: ContentBlock = serde_json::from_str(s).unwrap();
        match b {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content.as_text(), Some("done"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn message_text_or_blocks() {
        let m = Message::user(MessageContent::from_text("hello"));
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"role\":\"user\""));
        assert!(j.contains("\"content\":\"hello\""));
    }

    #[test]
    fn stop_reason_unknown_falls_back() {
        let j = serde_json::json!("something_new");
        let s: StopReason = serde_json::from_value(j).unwrap();
        assert_eq!(s, StopReason::Other("something_new".into()));
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::json!("something_new")
        );
    }
}
