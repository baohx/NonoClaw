//! Session MCP tool selection by keyword relevance.
//!
//! MCP servers can contribute many tools with large schemas, all of which land
//! in the `tools` array — a single contiguous prompt-cache block. Including
//! every MCP tool inflates the cached prefix. Unlike skill bodies, MCP tool
//! schemas cannot be "progressive-disclosed" (the API needs `input_schema` in
//! the array for the model to call the tool), so the cache-safe lever is to
//! narrow to a **relevant subset**, computed once per run from the user's
//! message and kept stable for the run. The tools array is built once per run
//! and reused across all tool-call rounds, so within-run caching is preserved.
//!
//! Policy (conservative — never narrows blindly):
//! - Built-in (non-`mcp__`) tools are always included.
//! - If there are `<= top_k` MCP tools, include all of them (no narrowing).
//! - Otherwise score MCP tools by keyword relevance to the message; include
//!   those with any signal, capped at `top_k`. If nothing matches, include all.
//! - Excluded MCP tools remain discoverable via `ToolSearch`.

use std::collections::HashSet;

use nonoclaw_tools::builtin::tool_search::ToolSearchEntry;

/// Default cap on how many MCP tools to advertise once narrowing applies.
pub const DEFAULT_TOP_K: usize = 15;

/// Relevance score for a tool entry against lowercased query tokens. Mirrors
/// ToolSearch's scoring (name exact > name substring > hint > description).
fn score_entry(tokens: &[String], entry: &ToolSearchEntry) -> i32 {
    let name = entry.name.to_lowercase();
    let desc = entry.description.to_lowercase();
    let hint = entry.search_hint.to_lowercase();
    let mut score: i32 = 0;
    for tok in tokens {
        if name == *tok {
            score += 100;
        } else if name.contains(tok) {
            score += 50;
        }
        if hint.contains(tok) {
            score += 30;
        }
        if desc.contains(tok) {
            score += 10;
        }
    }
    score
}

/// Select the MCP tool names to advertise for this run.
///
/// Returns `None` when no narrowing applies (too few MCP tools, empty message,
/// or no keyword match) — the caller then includes all MCP tools, preserving
/// the default behavior. Returns `Some(set)` only when narrowing is warranted;
/// the caller keeps non-MCP tools and, among MCP tools, only those in the set.
pub fn select_mcp_tools(
    user_text: &str,
    all_entries: &[ToolSearchEntry],
    top_k: usize,
) -> Option<HashSet<String>> {
    let mcp_entries: Vec<&ToolSearchEntry> = all_entries
        .iter()
        .filter(|e| e.name.starts_with("mcp__"))
        .collect();
    // Too few MCP tools to benefit from narrowing — keep them all.
    if mcp_entries.len() <= top_k {
        return None;
    }

    let tokens: Vec<String> = user_text
        .to_lowercase()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    if tokens.is_empty() {
        return None;
    }

    // Keep only MCP tools with any keyword signal.
    let mut scored: Vec<(&ToolSearchEntry, i32)> = mcp_entries
        .iter()
        .map(|e| (*e, score_entry(&tokens, e)))
        .filter(|(_, s)| *s > 0)
        .collect();
    if scored.is_empty() {
        // No keyword match — don't blindly narrow; keep all MCP tools.
        return None;
    }

    // Higher score first; name ascending as a stable tiebreak.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    Some(
        scored
            .into_iter()
            .take(top_k)
            .map(|(e, _)| e.name.clone())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, desc: &str, hint: &str) -> ToolSearchEntry {
        ToolSearchEntry {
            name: name.into(),
            description: desc.into(),
            search_hint: hint.into(),
        }
    }

    #[test]
    fn no_narrowing_when_few_mcp_tools() {
        // 2 MCP tools, top_k = 15 → no narrowing.
        let entries = vec![
            entry("mcp__srv__a", "alpha tool", ""),
            entry("mcp__srv__b", "beta tool", ""),
            entry("Read", "read files", ""),
        ];
        assert_eq!(select_mcp_tools("deploy the app", &entries, 15), None);
    }

    #[test]
    fn narrows_to_relevant_when_many_mcp_tools() {
        // 1 builtin + 1 relevant MCP + 15 filler MCP (> top_k 5).
        let mut entries = vec![entry("Read", "read files", "")];
        entries.push(entry("mcp__ci__deploy", "deploy the service", "deploy"));
        for i in 0..15 {
            // Filler descriptions intentionally share no tokens with the query.
            entries.push(entry(&format!("mcp__fill__t{i}"), "zzz noop filler", ""));
        }
        let sel = select_mcp_tools("please deploy the service", &entries, 5);
        let sel = sel.expect("should narrow to relevant MCP tools");
        assert!(sel.contains("mcp__ci__deploy"));
        assert!(!sel.contains("mcp__fill__t0"));
        assert!(sel.len() <= 5);
    }

    #[test]
    fn no_narrowing_when_no_keyword_match() {
        // Many MCP tools but the message shares no tokens → keep all.
        let mut entries = vec![entry("Read", "read", "")];
        for i in 0..16 {
            entries.push(entry(&format!("mcp__fill__t{i}"), "alpha capability", ""));
        }
        assert_eq!(select_mcp_tools("zzz nothing matches here", &entries, 5), None);
    }

    #[test]
    fn empty_message_keeps_all() {
        let mut entries = vec![entry("Read", "read", "")];
        for i in 0..16 {
            entries.push(entry(&format!("mcp__x__t{i}"), "cap", ""));
        }
        assert_eq!(select_mcp_tools("", &entries, 5), None);
    }

    #[test]
    fn ignores_non_mcp_tools_when_scoring() {
        // A built-in named "Deploy" must not be selected (only MCP names count).
        let mut entries = vec![entry("Deploy", "deploy stuff", "deploy")];
        for i in 0..16 {
            entries.push(entry(&format!("mcp__x__t{i}"), "zzz filler", ""));
        }
        // No MCP tool matched "deploy" → keep all (None).
        assert_eq!(select_mcp_tools("deploy please", &entries, 5), None);
    }
}
