//! Progressive tool-schema visibility.
//!
//! Only a small core set is advertised on every request. Additional built-in
//! and MCP tools are selected from the user's intent or explicitly activated
//! through ToolSearch. Execution permissions remain independent from schema
//! visibility.

use std::collections::HashSet;

use nonoclaw_tools::builtin::tool_search::ToolSearchEntry;

/// Default cap for intent-selected non-core schemas.
pub const DEFAULT_TOP_K: usize = 5;

/// Recovery-capable core set. ToolSearch is always inserted even when a custom
/// list accidentally omits it.
pub const DEFAULT_CORE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "ToolSearch",
    "SkillSearch",
    "Skill",
];

pub fn default_core_tools() -> Vec<String> {
    DEFAULT_CORE_TOOLS
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

/// Smaller always-visible recovery set used by `tokenMode=ultra`. Remaining
/// tools are intent-selected or activated through ToolSearch.
pub const ULTRA_CORE_TOOLS: &[&str] =
    &["Read", "Edit", "Bash", "ToolSearch", "SkillSearch", "Skill"];

pub fn ultra_core_tools() -> Vec<String> {
    ULTRA_CORE_TOOLS
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpNoMatchPolicy {
    #[default]
    None,
    Safe,
    All,
}

impl McpNoMatchPolicy {
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "safe" => Self::Safe,
            "all" => Self::All,
            _ => Self::None,
        }
    }
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Relevance score shared by first-request selection and ToolSearch semantics.
fn score_entry(tokens: &[String], entry: &ToolSearchEntry) -> i32 {
    let name = entry.name.to_lowercase();
    let description = entry.description.to_lowercase();
    let hint = entry.search_hint.to_lowercase();    let mut score = 0;
    for token in tokens {
        if name == *token {
            score += 100;
        } else if name.contains(token) {
            score += 50;
        }
        if hint.contains(token) {
            score += 30;
        }
        if description.contains(token) {
            score += 10;
        }
    }
    score
}

/// Compute the exact schema visibility set for a model request.
///
/// - core and explicitly activated tools are always visible;
/// - up to `top_k` additional tools are selected by intent;
/// - when no MCP tool has any relevance signal, the configured fallback is
///   applied (`none`, explicit safe allowlist, or legacy `all`);
/// - disabling MCP auto-selection preserves the legacy all-MCP behavior.
pub fn select_visible_tools(
    user_text: &str,
    all_entries: &[ToolSearchEntry],
    core_tools: &[String],
    top_k: usize,
    auto_select_mcp: bool,
    mcp_no_match_policy: McpNoMatchPolicy,
    safe_mcp_tools: &[String],
    activated_tools: &HashSet<String>,
) -> HashSet<String> {
    let mut visible: HashSet<String> = core_tools.iter().cloned().collect();
    visible.insert("ToolSearch".into());
    visible.extend(activated_tools.iter().cloned());

    let tokens = query_tokens(user_text);
    let mut scored = all_entries
        .iter()
        .filter(|entry| !visible.contains(&entry.name))
        .map(|entry| (entry, score_entry(&tokens, entry)))
        .filter(|(_, score)| *score > 0)
        .collect::<Vec<_>>();
    let has_mcp_match = scored
        .iter()
        .any(|(entry, _)| entry.name.starts_with("mcp__"));
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.0.name.cmp(&right.0.name))
    });
    visible.extend(
        scored
            .into_iter()
            .take(top_k)
            .map(|(entry, _)| entry.name.clone()),
    );

    let mcp_entries = all_entries
        .iter()
        .filter(|entry| entry.name.starts_with("mcp__"));
    if !auto_select_mcp {
        visible.extend(mcp_entries.map(|entry| entry.name.clone()));
    } else if !has_mcp_match {
        match mcp_no_match_policy {
            McpNoMatchPolicy::None => {}
            McpNoMatchPolicy::Safe => {
                let safe: HashSet<&str> = safe_mcp_tools.iter().map(String::as_str).collect();
                visible.extend(
                    mcp_entries
                        .filter(|entry| safe.contains(entry.name.as_str()))
                        .map(|entry| entry.name.clone()),
                );
            }
            McpNoMatchPolicy::All => {
                visible.extend(mcp_entries.map(|entry| entry.name.clone()));
            }
        }
    }

    visible
}

// ── Static MCP contract (AutoGenesis contract-generation borrow) ───────────
//
// The paper's context manager emits a static "skills.md-style contract" that
// summarises each registered resource once, instead of re-deriving per-turn
// descriptions. NonoClaw's equivalent: a merged one-line-per-MCP-tool
// contract document, cached in `.nonoclaw/mcp-contract.md` keyed by a content
// hash of the entry list. Regenerated only when the registry content hash
// changes (server add/remove). The per-request keyword scoring above stays —
// the contract is the stable, human/agent-readable inventory layer.

fn entry_list_hash(entries: &[ToolSearchEntry]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut mcp: Vec<&ToolSearchEntry> = entries
        .iter()
        .filter(|e| e.name.starts_with("mcp__"))
        .collect();
    mcp.sort_by(|a, b| a.name.cmp(&b.name));
    for e in mcp {
        e.name.hash(&mut hasher);
        e.search_hint.hash(&mut hasher);
        e.description.chars().take(200).collect::<String>().hash(&mut hasher);
    }
    hasher.finish()
}

/// Build the merged MCP contract document from the entry list.
pub fn build_mcp_contract(entries: &[ToolSearchEntry]) -> String {
    let mut mcp: Vec<&ToolSearchEntry> = entries
        .iter()
        .filter(|e| e.name.starts_with("mcp__"))
        .collect();
    mcp.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = String::from("# MCP Tool Contract\n\n");
    if mcp.is_empty() {
        out.push_str("(no MCP servers registered)\n");
        return out;
    }
    let mut current_server = String::new();
    for e in mcp {
        // name shape: mcp__<server>__<tool>
        let server = e
            .name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split("__").next())
            .unwrap_or("unknown");
        if server != current_server {
            current_server = server.to_string();
            out.push_str(&format!("\n## {server}\n\n"));
        }
        let hint = if e.search_hint.is_empty() {
            e.description.chars().take(120).collect::<String>()
        } else {
            e.search_hint.clone()
        };
        out.push_str(&format!("- `{}` — {}\n", e.name, hint));
    }
    out
}

/// Write the contract to `<cwd>/.nonoclaw/mcp-contract.md` when the entry
/// hash changed (content-hash invalidation, same pattern as the vector
/// index). Soft-fail; returns true when the file was refreshed.
pub fn refresh_mcp_contract(cwd: &std::path::Path, entries: &[ToolSearchEntry]) -> bool {
    use std::sync::Mutex;
    static LAST_HASH: std::sync::OnceLock<Mutex<u64>> = std::sync::OnceLock::new();
    let hash = entry_list_hash(entries);
    let cell = LAST_HASH.get_or_init(|| Mutex::new(0));
    if let Ok(mut seen) = cell.lock() {
        if *seen == hash {
            return false;
        }
        *seen = hash;
    }
    let dir = cwd.join(".nonoclaw");
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    std::fs::write(dir.join("mcp-contract.md"), build_mcp_contract(entries)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, description: &str, hint: &str) -> ToolSearchEntry {
        ToolSearchEntry {
            name: name.into(),
            description: description.into(),
            search_hint: hint.into(),
        }
    }

    fn core() -> Vec<String> {
        vec!["Read".into(), "ToolSearch".into()]
    }

    #[test]
    fn no_match_advertises_zero_mcp_by_default() {
        let entries = vec![
            entry("Read", "read files", ""),
            entry("mcp__ci__deploy", "deploy service", "deploy"),
        ];
        let visible = select_visible_tools(
            "explain this code",
            &entries,
            &core(),
            5,
            true,
            McpNoMatchPolicy::None,
            &[],
            &HashSet::new(),
        );
        assert!(!visible.iter().any(|name| name.starts_with("mcp__")));
    }

    #[test]
    fn relevant_tools_are_bounded_and_selected() {
        let mut entries = vec![entry("Read", "read files", "")];
        entries.push(entry("mcp__ci__deploy", "deploy service", "deploy"));
        entries.push(entry("DeployLocal", "deploy local build", "deploy"));
        for index in 0..10 {
            entries.push(entry(&format!("Other{index}"), "unrelated", ""));
        }
        let visible = select_visible_tools(
            "deploy service",
            &entries,
            &core(),
            2,
            true,
            McpNoMatchPolicy::None,
            &[],
            &HashSet::new(),
        );
        assert!(visible.contains("mcp__ci__deploy"));
        assert!(visible.contains("DeployLocal"));
        assert_eq!(visible.len(), core().len() + 2);
    }

    #[test]
    fn safe_and_all_fallbacks_are_explicit() {
        let entries = vec![
            entry("mcp__fs__read", "filesystem read", ""),
            entry("mcp__prod__delete", "production delete", ""),
        ];
        let safe = select_visible_tools(
            "hello",
            &entries,
            &core(),
            5,
            true,
            McpNoMatchPolicy::Safe,
            &["mcp__fs__read".into()],
            &HashSet::new(),
        );
        assert!(safe.contains("mcp__fs__read"));
        assert!(!safe.contains("mcp__prod__delete"));

        let all = select_visible_tools(
            "hello",
            &entries,
            &core(),
            5,
            true,
            McpNoMatchPolicy::All,
            &[],
            &HashSet::new(),
        );
        assert!(all.contains("mcp__fs__read"));
        assert!(all.contains("mcp__prod__delete"));
    }

    #[test]
    fn explicit_activation_survives_unrelated_intent() {
        let entries = vec![entry("mcp__db__query", "database query", "database")];
        let activated = HashSet::from(["mcp__db__query".to_string()]);
        let visible = select_visible_tools(
            "write documentation",
            &entries,
            &core(),
            1,
            true,
            McpNoMatchPolicy::None,
            &[],
            &activated,
        );
        assert!(visible.contains("mcp__db__query"));
    }



    #[test]
    fn mcp_contract_groups_by_server_and_lists_tools() {
        let entries = vec![
            entry("mcp__db__query", "run SQL query", "database"),
            entry("mcp__db__migrate", "migrate schema", "database"),
            entry("mcp__web__fetch", "fetch page", "web"),
        ];
        let c = build_mcp_contract(&entries);
        assert!(c.contains("# MCP Tool Contract"));
        assert!(c.contains("## db"));
        assert!(c.contains("## web"));
        assert!(c.find("mcp__db__query").unwrap() < c.find("mcp__web__fetch").unwrap());
        let builtin = vec![entry("Read", "read a file", "file")];
        assert!(build_mcp_contract(&builtin).contains("no MCP servers"));
    }

    #[test]
    fn contract_refresh_is_hash_gated() {
        let dir = std::env::temp_dir().join(format!("nc-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entries = vec![entry("mcp__db__query", "query", "db")];
        assert!(refresh_mcp_contract(&dir, &entries), "first call writes");
        assert!(!refresh_mcp_contract(&dir, &entries), "same hash is no-op");
        let changed = vec![entry("mcp__db__query", "query v2", "db"), entry("mcp__db__exec", "exec", "db")];
        assert!(refresh_mcp_contract(&dir, &changed), "content change rewrites");
        assert!(dir.join(".nonoclaw/mcp-contract.md").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
