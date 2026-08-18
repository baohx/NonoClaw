//! Token estimation. The authoritative source of token counts is the
//! provider's own `input_tokens` report (see `last_input_tokens` in the engine
//! loop). This module provides the estimate used only for the first turn
//! (before any provider data is available) and for budget allocation.
//!
//! Estimates are **exact** for model families with a known BPE encoding
//! (OpenAI, DeepSeek, Qwen, Moonshot/Kimi, Zhipu/GLM, Mistral, MiniMax — see
//! [`tiktoken::encoding_for_model`]) via a bundled pure-Rust implementation of
//! the real tokenizer; no runtime downloads. Unknown models fall back to a
//! content-type-aware chars/token heuristic (prose ~4, code ~3) with ~10-15%
//! error instead of the naive ~25%.

use tiktoken::CoreBpe;
use nonoclaw_core::{ContentBlock, Message, MessageContent};

/// Default chars-per-token for plain English prose. Code and JSON have
/// denser tokenization (~3 chars/token) due to symbols and short identifiers.
const CHARS_PER_TOKEN_PROSE: usize = 4;
/// Code, JSON, and structured content tokenize more finely.
const CHARS_PER_TOKEN_CODE: usize = 3;
/// Per-message structural overhead (role tags, separators).
const PER_MESSAGE_OVERHEAD: usize = 5;
const IMAGE_TOKENS: usize = 1200;

/// Real BPE encoding for a model family, when known.
///
/// Wraps `tiktoken::encoding_for_model` (bundled rank tables). `None` for
/// unknown models → callers fall back to the heuristic.
pub fn encoding_for_model(model: &str) -> Option<&'static CoreBpe> {
    tiktoken::encoding_for_model(model)
}

/// Exact BPE token count for `text` under the model's encoding, when known.
/// Falls back to the blended heuristic otherwise.
pub fn count_text_tokens(model: Option<&str>, text: &str) -> usize {
    if let Some(model) = model {
        if let Some(bpe) = encoding_for_model(model) {
            return bpe.encode(text).len();
        }
    }
    heuristic_text_tokens(text)
}

fn heuristic_text_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let cpt = blended_chars_per_token(text);
    (text.chars().count() as f64 / cpt).ceil() as usize
}

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
        ContentBlock::Image { .. } => IMAGE_TOKENS * CHARS_PER_TOKEN_PROSE,
    }
}

/// Estimate the "code weight" of a character string: what fraction looks like
/// code/JSON (brackets, semicolons, short tokens) vs prose. Returns a blended
/// chars-per-token ratio.
fn blended_chars_per_token(s: &str) -> f64 {
    if s.is_empty() {
        return CHARS_PER_TOKEN_PROSE as f64;
    }
    let code_indicators = s
        .chars()
        .filter(|c| matches!(c, '{' | '}' | '[' | ']' | '(' | ')' | ';' | '=' | '<' | '>' | '|' | '&' | '\\' | '/' | '*' | '#' | '$' | '@' | '`'))
        .count();
    let ratio = code_indicators as f64 / s.chars().count() as f64;
    // Interpolate between prose (4) and code (3) based on symbol density.
    // Typical code has ~8-15% symbols; prose has <2%.
    let blend = (ratio / 0.10).min(1.0);
    CHARS_PER_TOKEN_PROSE as f64 - blend * (CHARS_PER_TOKEN_PROSE - CHARS_PER_TOKEN_CODE) as f64
}

/// Estimated tokens for a single message. Uses the exact BPE tokenizer when
/// the model's encoding is known; falls back to the heuristic otherwise.
pub fn estimate_message_tokens_for_model(model: Option<&str>, m: &Message) -> usize {
    let model = model.filter(|m| !m.is_empty());
    match &m.content {
        MessageContent::Text(s) => count_text_tokens(model, s) + PER_MESSAGE_OVERHEAD,
        MessageContent::Blocks(blocks) => {
            // Individual blocks may have very different content types; use the
            // BPE path per text block and heuristic for structured blocks.
            let mut total = 0usize;
            for b in blocks {
                total += match b {
                    ContentBlock::Text { text, .. } => count_text_tokens(model, text),
                    ContentBlock::Thinking { thinking, .. } => count_text_tokens(model, thinking),
                    ContentBlock::ToolUse { name, input, .. } => {
                        count_text_tokens(model, name) + count_text_tokens(model, &input.to_string())
                    }
                    ContentBlock::ToolResult { content, .. } => match content {
                        nonoclaw_core::ToolResultContent::Text(s) => count_text_tokens(model, s),
                        nonoclaw_core::ToolResultContent::Blocks(bs) => bs
                            .iter()
                            .map(|inner| match inner {
                                ContentBlock::Text { text, .. } => count_text_tokens(model, text),
                                other => block_char_len(other) / CHARS_PER_TOKEN_CODE,
                            })
                            .sum(),
                    },
                    ContentBlock::Image { .. } => IMAGE_TOKENS,
                };
            }
            total + PER_MESSAGE_OVERHEAD
        }
    }
}

/// Estimated tokens for a single message using the heuristic (unknown model).
pub fn estimate_message_tokens(m: &Message) -> usize {
    estimate_message_tokens_for_model(None, m)
}

/// Estimated total prompt tokens: system text + tool schemas + all messages.
///
/// Text messages are counted with the exact BPE tokenizer when `model`'s
/// encoding is known. Tool schemas, system prompt, and block-structured
/// messages use the code-ish heuristic ratio. `chars_per_token` adjusts only
/// the fixed/structured portions (never text counted by BPE).
pub fn estimate_total_for_model(
    model: Option<&str>,
    messages: &[Message],
    system_chars: usize,
    tools_chars: usize,
    chars_per_token: usize,
) -> usize {
    let cpt = if chars_per_token == 0 {
        CHARS_PER_TOKEN_PROSE
    } else {
        chars_per_token
    };
    // Tool schemas and system prompt are structured content → use code ratio.
    let fixed_tokens = (system_chars + tools_chars) / cpt.min(CHARS_PER_TOKEN_CODE + 1);
    let body_tokens: usize = messages
        .iter()
        .map(|m| estimate_message_tokens_for_model(model, m))
        .sum();
    fixed_tokens + body_tokens
}

/// Estimated total using the heuristic only (callers without a model name).
pub fn estimate_total(
    messages: &[Message],
    system_chars: usize,
    tools_chars: usize,
    chars_per_token: usize,
) -> usize {
    estimate_total_for_model(None, messages, system_chars, tools_chars, chars_per_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_rounds_reasonably() {
        let m = Message::user(MessageContent::from_text("Hello world"));
        // 11 chars / ~4 cpt ≈ 2 + 5 overhead = ~7
        let est = estimate_message_tokens(&m);
        assert!((5..=9).contains(&est), "got {est}");
    }

    #[test]
    fn code_content_estimates_higher_than_prose() {
        // Same length, but code has more symbols → more tokens.
        let prose = Message::user(MessageContent::from_text("This is a sentence with words and more words here."));
        let code = Message::user(MessageContent::from_text("{(\"key\": value); [arr] = func(a, b, c); return x;}"));
        let prose_tokens = estimate_message_tokens(&prose);
        let code_tokens = estimate_message_tokens(&code);
        assert!(
            code_tokens >= prose_tokens,
            "code ({code_tokens}) should estimate >= prose ({prose_tokens}) for same-length text"
        );
    }

    #[test]
    fn tool_use_counts_input_json() {
        let m = Message::assistant(MessageContent::from_blocks(vec![ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "Read".into(),
            cache_control: None,
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
        let t1 = estimate_total(&[one], 1000, 500, 4);
        let t2 = estimate_total(&many, 1000, 500, 4);
        assert!(t2 > t1 * 6); // ~10x body minus fixed overhead → well above 6x
    }

    #[test]
    fn known_model_uses_real_bpe() {
        // "Hello world" tokenizes to 2 tokens under cl100k_base/o200k_base.
        let tokens = count_text_tokens(Some("gpt-4o"), "Hello world");
        assert_eq!(tokens, 2, "exact BPE count for gpt-4o");
    }

    #[test]
    fn deepseek_alias_resolves_to_bundled_encoding() {
        // deepseek-chat maps to the bundled deepseek_v4 encoding in tiktoken.
        assert!(encoding_for_model("deepseek-chat").is_some());
        let tokens = count_text_tokens(Some("deepseek-chat"), "Hello world");
        assert!(tokens > 0);
    }

    #[test]
    fn unknown_model_falls_back_to_heuristic() {
        // Unknown model name → no encoding → heuristic path still works.
        assert!(encoding_for_model("claude-opus-4-9999").is_none());
        let tokens = count_text_tokens(Some("claude-opus-4-9999"), "Hello world");
        assert!((2..=8).contains(&tokens));
    }

    #[test]
    fn bpe_vs_heuristic_agree_on_repeated_ascii() {
        // BPE on ASCII runs yields ~1 token/word; heuristic should be close.
        let text = "the quick brown fox jumps over the lazy dog ".repeat(10);
        let bpe = count_text_tokens(Some("gpt-4o"), &text);
        let heuristic = heuristic_text_tokens(&text);
        let diff = (bpe as i64 - heuristic as i64).unsigned_abs();
        let bound = (heuristic / 2 + 10) as u64;
        assert!(
            diff <= bound,
            "bpe={bpe} heuristic={heuristic} diff too large"
        );
    }

    #[test]
    fn estimate_total_for_model_counts_text_exactly() {
        let m = Message::user(MessageContent::from_text("Hello world"));
        let total = estimate_total_for_model(Some("gpt-4o"), &[m], 0, 0, 4);
        // 2 BPE tokens for text + 5 overhead = 7; fixed = 0.
        assert_eq!(total, 7, "got {total}");
    }
}
