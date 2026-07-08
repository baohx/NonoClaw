//! Rough token estimation. Mirrors the role of `src/services/tokenEstimation.ts`
//! (a per-language estimator); this is the classic ~4-chars-per-token heuristic
//! good enough to drive auto-compact thresholds, not billing.

use nonoclaw_core::{ContentBlock, Message, MessageContent};

const CHARS_PER_TOKEN: usize = 4;
const PER_MESSAGE_OVERHEAD: usize = 4; // tokens of structural overhead per message
const IMAGE_TOKENS: usize = 1200;

/// Total char length of a message's content (across all blocks).
pub fn message_char_len(m: &Message) -> usize {
    match &m.content {
        MessageContent::Text(s) => s.chars().count(),
        MessageContent::Blocks(blocks) => blocks.iter().map(block_char_len).sum(),
    }
}

fn block_char_len(b: &ContentBlock) -> usize {
    match b {
        ContentBlock::Text { text, .. } => text.chars().count(),
        ContentBlock::ToolUse { name, input, .. } => name.chars().count() + input.to_string().len(),
        ContentBlock::ToolResult { content, .. } => match content {
            nonoclaw_core::ToolResultContent::Text(s) => s.chars().count(),
            nonoclaw_core::ToolResultContent::Blocks(bs) => bs.iter().map(block_char_len).sum(),
        },
        ContentBlock::Thinking { thinking, .. } => thinking.chars().count(),
        ContentBlock::Image { .. } => IMAGE_TOKENS * CHARS_PER_TOKEN,
    }
}

/// Estimated tokens for a single message.
pub fn estimate_message_tokens(m: &Message) -> usize {
    message_char_len(m) / CHARS_PER_TOKEN + PER_MESSAGE_OVERHEAD
}

/// Estimated total prompt tokens: system text + tool schemas + all messages.
pub fn estimate_total(messages: &[Message], system_chars: usize, tools_chars: usize) -> usize {
    let body: usize = messages.iter().map(message_char_len).sum();
    (system_chars + tools_chars + body) / CHARS_PER_TOKEN + messages.len() * PER_MESSAGE_OVERHEAD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_rounds_reasonably() {
        let m = Message::user(MessageContent::from_text("Hello world"));
        // 11 chars / 4 ≈ 2 + 4 overhead = ~6
        assert!(estimate_message_tokens(&m) >= 4 && estimate_message_tokens(&m) <= 8);
    }

    #[test]
    fn tool_use_counts_input_json() {
        let m = Message::assistant(MessageContent::from_blocks(vec![ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "/a/very/long/path/to/some/file.rs"}),
        }]));
        assert!(estimate_message_tokens(&m) > 0);
    }

    #[test]
    fn image_is_a_fixed_cost() {
        let m = Message::user(MessageContent::from_blocks(vec![ContentBlock::Image {
            source: nonoclaw_core::ImageSource {
                kind: "base64".into(),
                media_type: "image/png".into(),
                data: String::new(),
            },
        }]));
        // image alone ≈ 1200 tokens
        assert!(estimate_message_tokens(&m) >= 1200);
    }

    #[test]
    fn total_scales_with_messages() {
        let one = Message::user(MessageContent::from_text("x".repeat(4000)));
        let many = vec![one.clone(); 10];
        let t1 = estimate_total(&[one], 1000, 500);
        let t2 = estimate_total(&many, 1000, 500);
        assert!(t2 > t1 * 6); // ~10x body minus fixed overhead → well above 6x
    }
}
