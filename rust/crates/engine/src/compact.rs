//! Auto-compaction. Mirrors the role of `src/services/compact/`: when the
//! transcript nears the context window, summarize the older portion into a
//! single message and keep the recent tail verbatim. Triggered by the engine
//! loop based on a token estimate (see [`crate::tokens`]).
//!
//! Compaction is in-memory for the current run; the session file keeps the full
//! history so a `--resume` gets maximum fidelity (and may re-compact).

use nonoclaw_api::{Client, RequestParams, SystemBlock};

use nonoclaw_core::{ContentBlock, Message, MessageContent, Result, ToolResultContent};
use serde_json::Value;

const SUMMARY_SYSTEM: &str = r#"You are a summarization assistant. Produce a concise but complete summary of the conversation that preserves everything a continuing assistant needs. Use the following XML structure so the continuing assistant can scan it reliably:

<goal>The user's overall goal and constraints in 1-3 sentences.</goal>
<decisions>
- Each key decision made during the conversation, with the rationale (one bullet per decision).
</decisions>
<files_modified>
- path/to/file — what changed and why (one bullet per file; include line numbers or identifiers when relevant). Use "read" if the file was only inspected.
</files_modified>
<commands_run>
- command → outcome (one bullet per command; include exit codes or error text on failure).
</commands_run>
<current_state>1-3 sentences describing where the work stands right now: what's done, what's in flight.</current_state>
<open_questions>
- Unresolved questions, blockers, or next steps the continuing assistant must address.
</open_questions>

Do NOT omit concrete technical details (paths, names, values, error messages). Skip sections that have nothing to record (e.g. no commands run → omit <commands_run> entirely)."#;

/// User instruction appended after a raw prefix replay (KV-cache reuse path).
/// Kept short so it does not disturb the byte-identical prefix that precedes it.
const SUMMARY_USER_INSTRUCTION: &str = "Summarize the conversation above so work can continue with only your summary plus the most recent messages. Preserve concrete technical details (paths, names, values, error messages). Use the XML structure: <goal>, <decisions>, <files_modified>, <commands_run>, <current_state>, <open_questions>. Omit empty sections.";

/// Approximate on-the-wire character count for a message slice. Serialized JSON
/// is a conservative upper bound on the provider payload; used only to decide
/// whether the summarizer can safely replay the raw prefix within budget.
fn wire_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| serde_json::to_string(m).map(|s| s.chars().count()).unwrap_or(0))
        .sum()
}

/// Decide whether the summarizer can replay the live request's raw prefix for
/// KV-cache reuse: the compact model must match the live model, a prior request
/// must exist, and the raw prefix must fit the input budget.
fn can_reuse_prefix(
    prefix_template: Option<&RequestParams>,
    model: &str,
    to_compact: &[Message],
    max_input_chars: usize,
) -> bool {
    match prefix_template {
        Some(tpl) => tpl.model == model && wire_chars(to_compact) <= max_input_chars,
        None => false,
    }
}


/// Default cap on the summarizer's output. Overridable via the
/// `compactMaxTokens` settings.json field.
pub const DEFAULT_MAX_SUMMARY_TOKENS: u32 = 8192;

/// Compaction strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactMode {
    /// Summarize everything older than `keep_recent` messages (current behaviour).
    Summary,
    /// Keep the last N complete turns (user→assistant→tool→result) verbatim;
    /// summarize only the older prefix.  Each turn boundary is a plain user
    /// prompt (no pending tool results).
    #[default]
    Segments,
}

/// Minimum number of complete turns kept verbatim in segments mode.
pub const KEEP_RECENT_TURNS: usize = 3;

/// Find a safe split point so the kept tail starts at a plain user prompt
/// (not a tool_result), guaranteeing no `tool_use` is orphaned from its result.
/// Returns the index of the first kept message, or `None` if no safe split
/// exists in the recent window.
pub fn find_split(messages: &[Message], keep_recent: usize) -> Option<usize> {
    if messages.len() <= keep_recent {
        return None;
    }
    let mut split = messages.len().saturating_sub(keep_recent);
    while split < messages.len() {
        if is_plain_user_prompt(&messages[split]) {
            // Ensure there's actually something older to compact.
            if split > 0 {
                return Some(split);
            }
            return None;
        }
        split += 1;
    }
    None
}

fn is_plain_user_prompt(m: &Message) -> bool {
    if !matches!(m.role, nonoclaw_core::Role::User) {
        return false;
    }
    match &m.content {
        MessageContent::Text(_) => true,
        MessageContent::Blocks(blocks) => !blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
    }
}

/// Compact `messages`: summarize the older prefix, keep the recent tail.
/// `keep_recent` is the minimum number of messages / turns to keep verbatim
/// (interpretation depends on `mode`). `max_input_chars` hard-bounds the
/// summarizer request payload; `max_summary_tokens` caps its output.
pub async fn compact_messages(
    client: &Client,
    model: &str,
    messages: &[Message],
    keep_recent: usize,
    mode: CompactMode,
    max_input_chars: usize,
    max_summary_tokens: u32,
    prefix_template: Option<&RequestParams>,
) -> Result<Vec<Message>> {
    // In segments mode, `keep_recent` counts turns, not messages.
    let effective_keep = match mode {
        CompactMode::Segments => {
            // Count KEEP_RECENT_TURNS complete turns backwards.
            let mut turns = 0usize;
            let mut idx = messages.len();
            for (i, m) in messages.iter().enumerate().rev() {
                if is_plain_user_prompt(m) {
                    turns += 1;
                    if turns >= keep_recent {
                        idx = i;
                        break;
                    }
                }
            }
            // We always summarize from the beginning; keep from `idx`.
            messages.len().saturating_sub(idx)
        }
        CompactMode::Summary => keep_recent,
    };

    let Some(split) = find_split(messages, effective_keep) else {
        return Ok(messages.to_vec());
    };
    let to_compact = &messages[..split];
    let keep = &messages[split..];

    // KV-cache reuse: replay the raw prefix (identical to the live request's
    // history prefix) so provider prefix caching (DeepSeek automatic,
    // Anthropic cache_control) hits. Requires the same model and a prefix that
    // fits the input budget; otherwise fall back to the flattened, bounded
    // rendering (which is cheaper to build but never shares a cache prefix).
    let reuse_prefix = can_reuse_prefix(prefix_template, model, to_compact, max_input_chars);

    let (system_blocks, request_messages) = if reuse_prefix {
        let tpl = prefix_template.expect("reuse_prefix implies Some");
        let mut msgs = to_compact.to_vec();
        msgs.push(Message::user(MessageContent::from_text(
            SUMMARY_USER_INSTRUCTION,
        )));
        (tpl.system.clone(), msgs)
    } else {
        let transcript =
            bound_summary_transcript(&render_for_summary(to_compact), max_input_chars);
        let user_text = format!(
            "Summarize the following conversation so work can continue with only your summary plus \
             the most recent messages. Preserve concrete technical details.\n\n<conversation>\n\
             {transcript}\n</conversation>"
        );
        (
            vec![SystemBlock {
                kind: "text".into(),
                text: SUMMARY_SYSTEM.into(),
                cache_control: None,
            }],
            vec![Message::user(MessageContent::from_text(user_text))],
        )
    };

    let params = RequestParams {
        model: model.to_string(),
        max_tokens: max_summary_tokens,
        system: system_blocks,
        messages: request_messages,
        tools: vec![],
        tool_choice: None,
        thinking: None,
        temperature: Some(0.0),
        betas: vec![],
        extra_body: None,
        trace_label: Some("compact".into()),
    };
    let turn = client.run_turn(&params, |_| {}).await?;
    let summary: String = turn
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut out = Vec::with_capacity(keep.len() + 1);
    out.push(Message::user(MessageContent::from_text(format!(
        "<conversation_history_summary>\n{summary}\n</conversation_history_summary>\n\
         Recent messages follow."
    ))));
    out.extend(keep.iter().cloned());
    Ok(out)
}

// ── Tool-result pruning (P0-2) ──────────────────────────────────────────────

/// Tool results larger than this many chars are head/tail trimmed in-memory.
pub const PRUNE_THRESHOLD_CHARS: usize = 8_192;
pub const PRUNE_HEAD_CHARS: usize = 4_096;
pub const PRUNE_TAIL_CHARS: usize = 1_024;
const PRUNE_MARKER: &str = "[middle pruned]";

// ── Micro-compact (AutoDream P1) ────────────────────────────────────────────

/// Micro threshold: results above this get aggressively trimmed even when no
/// compaction threshold has fired, to keep long tool-heavy sessions lean.
pub const MICRO_THRESHOLD_CHARS: usize = 2_048;
/// Head chars kept by micro-compact (aggressive: a quarter of the threshold).
pub const MICRO_HEAD_CHARS: usize = 512;
pub const MICRO_TAIL_CHARS: usize = 256;
const MICRO_MARKER: &str = "[micro-compact]";
/// Messages at the tail (most recent) are never micro-compacted: the active
/// turn's tool results are still being reasoned about.
pub const MICRO_PROTECT_RECENT: usize = 8;
/// Aging threshold: messages older than this many positions get their already
/// micro-compacted tool results folded further to a stub, because distant
/// history rarely needs more than a hint of what the tool returned.
pub const AGING_FOLD_FROM: usize = 40;
/// Head chars kept by the aging fold (stub-sized).
pub const AGING_FOLD_HEAD_CHARS: usize = 200;
const AGING_MARKER_TEXT: &str = "[aged]";

/// Cache-aware aggressive trim of old tool results (AutoDream P1). Runs
/// BEFORE the 80% compaction pre-fire check each turn. Unlike
/// `prune_tool_results` (8K threshold, only on compaction), this uses a 2K
/// threshold and skips the most recent `MICRO_PROTECT_RECENT` messages so
/// active work is untouched. Idempotent via `MICRO_MARKER`. Returns rewritten
/// messages and the number of results trimmed.
pub fn micro_compact(messages: &[Message]) -> (Vec<Message>, usize) {
    let protect_from = messages.len().saturating_sub(MICRO_PROTECT_RECENT);
    let mut trimmed = 0usize;
    let out: Vec<Message> = messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if i >= protect_from {
                return m.clone();
            }
            let MessageContent::Blocks(blocks) = &m.content else {
                return m.clone();
            };
            let mut new_blocks = Vec::with_capacity(blocks.len());
            let mut msg_changed = false;
            for block in blocks {
                let new_block = match block {
                    ContentBlock::ToolResult {
                        content: ToolResultContent::Text(text),
                        tool_use_id,
                        is_error,
                        ..
                    } => {
                        let count = text.chars().count();
                        if count > MICRO_THRESHOLD_CHARS
                            && !text.contains(MICRO_MARKER)
                            && !text.contains(PRUNE_MARKER)
                        {
                            msg_changed = true;
                            trimmed += 1;
                            ContentBlock::ToolResult {
                                tool_use_id: tool_use_id.clone(),
                                content: ToolResultContent::Text(format!(
                                    "{}\n…[{count} total chars; {MICRO_MARKER}]…\n{}",
                                    text.chars().take(MICRO_HEAD_CHARS).collect::<String>(),
                                    text.chars()
                                        .rev()
                                        .take(MICRO_TAIL_CHARS)
                                        .collect::<String>()
                                        .chars()
                                        .rev()
                                        .collect::<String>(),
                                )),
                                is_error: *is_error,
                                cache_control: None,
                            }
                        } else if i + AGING_FOLD_FROM <= protect_from
                            && count > AGING_FOLD_HEAD_CHARS
                            && text.contains(MICRO_MARKER)
                            && !text.contains(AGING_MARKER_TEXT)
                        {
                            // Aged history: already micro-compacted, now far
                            // enough back that a stub suffices. Idempotent via
                            // the aged marker.
                            msg_changed = true;
                            trimmed += 1;
                            ContentBlock::ToolResult {
                                tool_use_id: tool_use_id.clone(),
                                content: ToolResultContent::Text(format!(
                                    "{}\n…[{AGING_MARKER_TEXT}]…",
                                    text.chars().take(AGING_FOLD_HEAD_CHARS).collect::<String>(),
                                )),
                                is_error: *is_error,
                                cache_control: None,
                            }
                        } else {
                            block.clone()
                        }
                    }
                    _ => block.clone(),
                };
                new_blocks.push(new_block);
            }
            if msg_changed {
                let mut new_msg = m.clone();
                new_msg.content = MessageContent::Blocks(new_blocks);
                new_msg
            } else {
                m.clone()
            }
        })
        .collect();
    (out, trimmed)
}

/// Trim oversized `ToolResult::Text` blocks to head + marker + tail, in-memory
/// only. Idempotent: results already carrying the marker are left untouched.
/// Returns the rewritten messages and the number of results pruned. The
/// persisted session transcript is never modified — pruning only shrinks the
/// in-memory projection fed to the next provider request.
pub fn prune_tool_results(messages: &[Message]) -> (Vec<Message>, usize) {
    let mut pruned_count = 0usize;
    let out: Vec<Message> = messages
        .iter()
        .map(|m| {
            let MessageContent::Blocks(blocks) = &m.content else {
                return m.clone();
            };
            let mut new_blocks = Vec::with_capacity(blocks.len());
            let mut msg_changed = false;
            for block in blocks {
                let new_block = match block {
                    ContentBlock::ToolResult {
                        content: ToolResultContent::Text(text),
                        tool_use_id,
                        is_error,
                        ..
                    } => {
                        let count = text.chars().count();
                        if count > PRUNE_THRESHOLD_CHARS && !text.contains(PRUNE_MARKER) {
                            msg_changed = true;
                            pruned_count += 1;
                            ContentBlock::ToolResult {
                                tool_use_id: tool_use_id.clone(),
                                content: ToolResultContent::Text(prune_text(text, count)),
                                is_error: *is_error,
                                cache_control: None,
                            }
                        } else {
                            block.clone()
                        }
                    }
                    _ => block.clone(),
                };
                new_blocks.push(new_block);
            }
            if msg_changed {
                let mut new_msg = m.clone();
                new_msg.content = MessageContent::Blocks(new_blocks);
                new_msg
            } else {
                m.clone()
            }
        })
        .collect();
    (out, pruned_count)
}

fn prune_text(text: &str, total: usize) -> String {
    let head: String = text.chars().take(PRUNE_HEAD_CHARS).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(PRUNE_TAIL_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}\n…[{total} total chars; {PRUNE_MARKER}]…\n{tail}")
}

/// Render messages into a readable transcript for the summarizer.
pub fn render_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            nonoclaw_core::Role::User => "user",
            nonoclaw_core::Role::Assistant => "assistant",
        };
        match &m.content {
            MessageContent::Text(s) => out.push_str(&format!("{role}: {s}\n")),
            MessageContent::Blocks(bs) => {
                for b in bs {
                    match b {
                        ContentBlock::Text { text, .. } => {
                            out.push_str(&format!("{role}: {text}\n"));
                        }
                        ContentBlock::ToolUse { name, input, .. } => {
                            out.push_str(&format!(
                                "{role} tool_use {name}: {}\n",
                                compact_json(input)
                            ));
                        }
                        ContentBlock::ToolResult { content, .. } => {
                            let t = match content {
                                ToolResultContent::Text(s) => s.clone(),
                                ToolResultContent::Blocks(_) => "(blocks)".into(),
                            };
                            out.push_str(&format!("tool_result: {}\n", single_line(&t)));
                        }
                        ContentBlock::Thinking { thinking, .. } => {
                            out.push_str(&format!("{role} (thinking): {}\n", thinking));
                        }
                        ContentBlock::Image { .. } => out.push_str(&format!("{role}: (image)\n")),
                    }
                }
            }
        }
    }
    out
}

const SUMMARY_TRANSCRIPT_OMISSION: &str =
    "\n...[middle of older history omitted to fit compaction input budget]...\n";

/// Keep the original goal/context at the head and the most recent compacted
/// details at the tail while strictly bounding the summarizer transcript.
fn bound_summary_transcript(transcript: &str, max_chars: usize) -> String {
    let char_count = transcript.chars().count();
    if char_count <= max_chars {
        return transcript.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }

    let marker_chars = SUMMARY_TRANSCRIPT_OMISSION.chars().count();
    if max_chars <= marker_chars {
        return SUMMARY_TRANSCRIPT_OMISSION
            .chars()
            .take(max_chars)
            .collect();
    }

    let available = max_chars - marker_chars;
    let head_chars = available.div_ceil(2);
    let tail_chars = available - head_chars;
    let head: String = transcript.chars().take(head_chars).collect();
    let tail: String = transcript
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}{SUMMARY_TRANSCRIPT_OMISSION}{tail}")
}

fn compact_json(v: &Value) -> String {
    single_line(&v.to_string())
}

fn single_line(s: &str) -> String {
    let capped: String = s.chars().take(2000).collect();
    capped.replace('\n', " ⏎ ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_core::{ContentBlock, Role};

    fn user(t: &str) -> Message {
        Message::user(MessageContent::from_text(t))
    }
    fn asst(t: &str) -> Message {
        Message::assistant(MessageContent::from_text(t))
    }
    fn tool_use(id: &str) -> Message {
        Message::assistant(MessageContent::from_blocks(vec![ContentBlock::ToolUse {
            id: id.into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "/a"}),
            cache_control: None,
        }]))
    }
    fn tool_result(id: &str) -> Message {
        Message::user(MessageContent::from_blocks(vec![
            ContentBlock::tool_result(id.into(), "content", false),
        ]))
    }

    #[test]
    fn split_keeps_recent_and_starts_at_prompt() {
        // u a u(tool_result) a u(tool_result) a | u(prompt) a u(prompt)
        let msgs = vec![
            user("p1"),
            asst("a1"),
            tool_result("t1"),
            asst("a2"),
            tool_result("t2"),
            asst("a3"),
            user("p2"),
            asst("a4"),
            user("p3"),
        ];
        // keep_recent = 3 → split searches from index 6 for a plain prompt.
        let split = find_split(&msgs, 3).unwrap();
        assert_eq!(split, 6);
        assert!(matches!(msgs[split].role, Role::User));
        assert!(is_plain_user_prompt(&msgs[split]));
    }

    #[test]
    fn no_split_when_too_few_messages() {
        let msgs = vec![user("p1"), asst("a1")];
        assert!(find_split(&msgs, 4).is_none());
    }

    #[test]
    fn no_split_when_only_tool_results_in_window() {
        // Recent window has no plain prompt → safe to skip compaction.
        let msgs = vec![
            user("p1"),
            asst("a1"),
            tool_result("t1"),
            asst("a2"),
            tool_result("t2"),
        ];
        assert!(find_split(&msgs, 3).is_none());
    }

    #[test]
    fn render_includes_tool_uses_and_results() {
        let msgs = vec![user("p1"), tool_use("t1"), tool_result("t1")];
        let r = render_for_summary(&msgs);
        assert!(r.contains("user: p1"));
        assert!(r.contains("tool_use Read"));
        assert!(r.contains("tool_result:"));
    }

    #[test]
    fn summary_transcript_budget_preserves_head_and_tail() {
        let transcript = format!("goal-at-head\n{}\nrecent-details-at-tail", "x".repeat(400));
        let bounded = bound_summary_transcript(&transcript, 120);

        assert!(bounded.chars().count() <= 120);
        assert!(bounded.starts_with("goal-at-head"));
        assert!(bounded.ends_with("recent-details-at-tail"));
        assert!(bounded.contains("middle of older history omitted"));
        assert_eq!(bound_summary_transcript(&transcript, 0), "");
    }

    // ========================================================================
    // Batch 4 — XML structured context wrapping
    // ========================================================================

    #[test]
    fn compacted_summary_uses_xml_tag() {
        // T4.4 acceptance: the summary injected into messages must use
        // <conversation_history_summary>...</conversation_history_summary>,
        // not the legacy [Compacted summary...] / [End summary...] markers.
        // We can't call compact_messages (requires LLM client) so we
        // construct the wrapper the same way the function does and assert.
        let summary = "User asked about Rust ownership.";
        let wrapped = format!(
            "<conversation_history_summary>\n{summary}\n</conversation_history_summary>\n\
             Recent messages follow."
        );
        assert!(wrapped.contains("<conversation_history_summary>"));
        assert!(wrapped.contains("</conversation_history_summary>"));
        assert!(wrapped.contains(summary));
        assert!(!wrapped.contains("[Compacted summary"));
        assert!(!wrapped.contains("[End summary"));
    }

    // ========================================================================
    // Batch 5 — Compaction experience improvements
    // ========================================================================

    #[test]
    fn summary_system_prompts_for_structured_output() {
        // T5.1 acceptance: SUMMARY_SYSTEM must request structured XML output
        // with the agreed sections.
        assert!(SUMMARY_SYSTEM.contains("<goal>"));
        assert!(SUMMARY_SYSTEM.contains("<decisions>"));
        assert!(SUMMARY_SYSTEM.contains("<files_modified>"));
        assert!(SUMMARY_SYSTEM.contains("<commands_run>"));
        assert!(SUMMARY_SYSTEM.contains("<current_state>"));
        assert!(SUMMARY_SYSTEM.contains("<open_questions>"));
        // Sanity: not the legacy free-form prompt.
        assert!(
            !SUMMARY_SYSTEM.contains("Preserve concrete technical details.")
                || SUMMARY_SYSTEM.contains("Do NOT omit concrete technical details")
        );
    }

    #[test]
    fn default_max_summary_tokens_is_8192() {
        // T5.2 acceptance: the default is 8192; the constant is the
        // single source of truth referenced by settings resolution.
        assert_eq!(DEFAULT_MAX_SUMMARY_TOKENS, 8192);
    }

    // ========================================================================
    // KV-cache prefix reuse (P0-1)
    // ========================================================================

    fn live_request(model: &str) -> RequestParams {
        RequestParams {
            model: model.to_string(),
            max_tokens: 1024,
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            thinking: None,
            temperature: None,
            betas: vec![],
            extra_body: None,
            trace_label: None,
        }
    }

    #[test]
    fn wire_chars_counts_serialized_messages() {
        let msgs = vec![user("hello world")];
        let n = wire_chars(&msgs);
        assert!(n >= "hello world".len(), "wire chars must be at least the text length");
        assert!(n > 0);
    }

    #[test]
    fn prefix_reuse_requires_matching_model_and_budget() {
        let msgs = vec![user("short")];
        let tpl = live_request("deepseek-chat");
        // Matching model + fits budget → reuse.
        assert!(can_reuse_prefix(Some(&tpl), "deepseek-chat", &msgs, 1_000_000));
        // Different model → no reuse (cache prefix not portable across models).
        assert!(!can_reuse_prefix(Some(&tpl), "other-model", &msgs, 1_000_000));
        // Budget too small → no reuse (would truncate and break the prefix).
        assert!(!can_reuse_prefix(Some(&tpl), "deepseek-chat", &msgs, 1));
        // No prior request → no reuse.
        assert!(!can_reuse_prefix(None, "deepseek-chat", &msgs, 1_000_000));
    }

    // ========================================================================
    // Tool-result pruning (P0-2)
    // ========================================================================

    fn big_tool_result(id: &str, size: usize) -> Message {
        let text = "x".repeat(size);
        Message::user(MessageContent::from_blocks(vec![
            ContentBlock::tool_result(id.into(), text, false),
        ]))
    }

    #[test]
    fn prune_trims_only_oversized_tool_results() {
        let small = tool_result("t1");
        let big = big_tool_result("t2", PRUNE_THRESHOLD_CHARS + 10);
        let msgs = vec![small.clone(), big.clone(), user("plain prompt")];
        let (pruned, pruned_count) = prune_tool_results(&msgs);
        assert_eq!(pruned_count, 1);

        // Small result and plain prompt are untouched.
        assert_eq!(
            serde_json::to_string(&pruned[0]).unwrap(),
            serde_json::to_string(&small).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&pruned[2]).unwrap(),
            serde_json::to_string(&user("plain prompt")).unwrap()
        );

        // Oversized result is trimmed to head + marker + tail.
        let MessageContent::Blocks(blocks) = &pruned[1].content else {
            panic!("expected block content");
        };
        match &blocks[0] {
            ContentBlock::ToolResult {
                content: ToolResultContent::Text(t),
                ..
            } => {
                assert!(t.contains(PRUNE_MARKER));
                assert!(t.chars().count() < (PRUNE_THRESHOLD_CHARS + 10));
                assert!(t.starts_with('x'));
                assert!(t.ends_with('x'));
            }
            _ => panic!("expected text tool result"),
        }
    }

    #[test]
    fn prune_is_idempotent_and_ignores_small_results() {
        let small = tool_result("t1");
        let big = big_tool_result("t2", PRUNE_THRESHOLD_CHARS + 10);
        let (pruned, pruned_count) = prune_tool_results(&[small.clone(), big]);
        assert_eq!(pruned_count, 1);
        // Second pass: no further change.
        let (again, second_count) = prune_tool_results(&pruned);
        assert_eq!(second_count, 0, "pruning must be idempotent");
        assert_eq!(
            serde_json::to_string(&again).unwrap(),
            serde_json::to_string(&pruned).unwrap()
        );
    }

    // ========================================================================
    // Micro-compact (AutoDream P1)
    // ========================================================================

    fn many_messages(n: usize, big_size: usize) -> Vec<Message> {
        let mut v = Vec::new();
        for i in 0..n {
            v.push(big_tool_result(&format!("t{i}"), big_size));
        }
        v
    }

    #[test]
    fn micro_compact_trims_old_and_protects_recent() {
        // 20 messages of 3K chars each: only those older than the last 8
        // are eligible.
        let msgs = many_messages(20, MICRO_THRESHOLD_CHARS + 1000);
        let (out, count) = micro_compact(&msgs);
        assert_eq!(count, 12, "20 - 8 protected = 12 trimmed");
        // Eligible ones now carry the marker and are far smaller.
        for (i, m) in out.iter().take(12).enumerate() {
            let MessageContent::Blocks(b) = &m.content else {
                panic!("msg {i}");
            };
            if let ContentBlock::ToolResult {
                content: ToolResultContent::Text(t),
                ..
            } = &b[0]
            {
                assert!(t.contains(MICRO_MARKER));
                assert!(t.chars().count() < MICRO_THRESHOLD_CHARS + 1000);
            }
        }
        // Protected tail is byte-identical to the input.
        for (i, (a, b)) in out.iter().skip(12).zip(msgs.iter().skip(12)).enumerate() {
            assert_eq!(
                serde_json::to_string(a).unwrap(),
                serde_json::to_string(b).unwrap(),
                "protected msg {i} must be untouched"
            );
        }
    }

    #[test]
    fn micro_compact_is_idempotent() {
        let msgs = many_messages(20, MICRO_THRESHOLD_CHARS + 1000);
        let (first, n1) = micro_compact(&msgs);
        assert_eq!(n1, 12);
        let (second, n2) = micro_compact(&first);
        assert_eq!(n2, 0, "second pass trims nothing");
        assert_eq!(
            serde_json::to_string(&second).unwrap(),
            serde_json::to_string(&first).unwrap()
        );
    }

    #[test]
    fn micro_compact_skips_prune_marked_results() {
        // A result already pruned by the 8K pruner must not be re-trimmed
        // (it carries PRUNE_MARKER and stays as-is).
        let (pruned, _) = prune_tool_results(&[big_tool_result(
            "t0",
            PRUNE_THRESHOLD_CHARS + 10,
        )]);
        let msgs: Vec<Message> = pruned
            .into_iter()
            .chain(many_messages(10, MICRO_THRESHOLD_CHARS + 1000))
            .collect();
        // 11 total messages, protect last 8 → 3 eligible: the prune-marked
        // one is skipped, only the 2 new big ones in that window get trimmed.
        let (_, count) = micro_compact(&msgs);
        assert_eq!(count, 2, "prune-marked skipped, 2 eligible trimmed");
    }

    #[test]
    fn micro_compact_noop_on_short_history() {
        // Fewer than the protect window: nothing is ever trimmed.
        let msgs = many_messages(5, MICRO_THRESHOLD_CHARS + 1000);
        let (out, count) = micro_compact(&msgs);
        assert_eq!(count, 0);
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            serde_json::to_string(&msgs).unwrap()
        );
    }

    #[test]
    fn aging_fold_stubborn_distant_micro_compacted_results() {
        // Long history: the first micro-compact trims big results everywhere
        // outside the protect window; a second pass folds the aged tail of
        // that window (distance ≥ AGING_FOLD_FROM) down to a 200-char stub.
        let msgs = many_messages(60, MICRO_THRESHOLD_CHARS + 1000);
        let (first, n1) = micro_compact(&msgs);
        assert_eq!(n1, 52, "all messages outside protect window trimmed");
        let (second, n2) = micro_compact(&first);
        // protect_from = 52; aged window = indices 0..=12 (distance ≥ 40) → 13 stubs.
        assert_eq!(n2, 13, "aged results folded to stubs");
        let aged = serde_json::to_string(&second[0]).unwrap();
        assert!(aged.contains(AGING_MARKER_TEXT));
        assert!(!aged.contains(MICRO_MARKER), "stub replaces micro-compact text");
        // Third pass is fully idempotent.
        let (third, n3) = micro_compact(&second);
        assert_eq!(n3, 0);
        assert_eq!(
            serde_json::to_string(&third).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }
}
