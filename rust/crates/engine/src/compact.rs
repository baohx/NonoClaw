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

const SUMMARY_SYSTEM: &str = "You are a summarization assistant. Produce a concise but complete summary of the conversation that preserves everything a continuing assistant needs: the user's goal and constraints, key decisions and their rationale, files read or modified (with paths and the important snippets), commands run and their outcomes, the current state of work, and any open questions or next steps. Do NOT omit concrete technical details (paths, names, values).";
const MAX_SUMMARY_TOKENS: u32 = 4096;

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
/// `keep_recent` is the minimum number of messages to keep verbatim.
pub async fn compact_messages(
    client: &Client,
    model: &str,
    messages: &[Message],
    keep_recent: usize,
) -> Result<Vec<Message>> {
    let Some(split) = find_split(messages, keep_recent) else {
        return Ok(messages.to_vec());
    };
    let to_compact = &messages[..split];
    let keep = &messages[split..];

    let transcript = render_for_summary(to_compact);
    let user_text = format!(
        "Summarize the following conversation so work can continue with only your summary plus \
         the most recent messages. Preserve concrete technical details.\n\n<conversation>\n\
         {transcript}\n</conversation>"
    );
    let params = RequestParams {
        model: model.to_string(),
        max_tokens: MAX_SUMMARY_TOKENS,
        system: vec![SystemBlock {
            kind: "text".into(),
            text: SUMMARY_SYSTEM.into(),
            cache_control: None,
        }],
        messages: vec![Message::user(MessageContent::from_text(user_text))],
        tools: vec![],
        tool_choice: None,
        thinking: None,
        temperature: Some(0.0),
        betas: vec![],
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
        "[Compacted summary of earlier conversation]\n{summary}\n\
         [End summary — recent messages follow.]"
    ))));
    out.extend(keep.iter().cloned());
    Ok(out)
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
}
