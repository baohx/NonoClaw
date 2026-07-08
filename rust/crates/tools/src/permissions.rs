//! Permission engine. Mirrors the rule/mode logic in `src/utils/permissions/`
//! (`getNextPermissionMode`, the rule store, filesystem/bash classifiers).
//!
//! Phase 0 implements:
//!   * mode-based gating (`default` / `acceptEdits` / `auto` / `bypassPermissions`
//!     / `plan`),
//!   * allow/disallow tool-name patterns from the CLI,
//!   * composition with each tool's own `check_permissions`.
//!
//! The ML bash command classifier (`bashClassifier`) is **stubbed** — Bash
//! falls back to its own rule + the mode decision.

use nonoclaw_core::{PermissionDecision, PermissionMode, PermissionResult};

/// A configured permission gate for a session.
#[derive(Debug, Clone, Default)]
pub struct PermissionGate {
    pub mode: PermissionMode,
    pub allowed: Vec<String>,
    pub disallowed: Vec<String>,
}

impl PermissionGate {
    pub fn new(mode: PermissionMode, allowed: Vec<String>, disallowed: Vec<String>) -> Self {
        PermissionGate {
            mode,
            allowed,
            disallowed,
        }
    }

    /// Compose the gate's decision with a tool's `check_permissions` outcome.
    /// `is_read_only` is the tool's claim about this input.
    pub fn decide(
        &self,
        tool_name: &str,
        is_read_only: bool,
        tool_decision: &PermissionResult,
    ) -> PermissionDecision {
        // 1. Explicit disallow wins.
        if self
            .disallowed
            .iter()
            .any(|p| pattern_matches(p, tool_name))
        {
            return PermissionDecision::deny(format!("{tool_name} disallowed by config"));
        }

        // 2. Bypass skips everything else.
        if self.mode == PermissionMode::BypassPermissions {
            return PermissionDecision::allow();
        }

        // 3. Explicit allow.
        if self.allowed.iter().any(|p| pattern_matches(p, tool_name)) {
            return PermissionDecision::allow();
        }

        // 4. Plan mode: reads allowed, writes denied.
        if self.mode == PermissionMode::Plan {
            return if is_read_only {
                PermissionDecision::allow()
            } else {
                PermissionDecision::deny("write blocked in plan mode")
            };
        }

        // 5. AcceptEdits auto-approves edits + reads.
        if self.mode == PermissionMode::AcceptEdits {
            return if is_read_only || tool_decision.is_allow() {
                PermissionDecision::allow()
            } else {
                PermissionDecision::ask(format!("{tool_name} needs permission"))
            };
        }

        // 6. Auto mode auto-approves what the tool/rule engine allows.
        if self.mode == PermissionMode::Auto {
            return if tool_decision.is_allow() {
                PermissionDecision::allow()
            } else {
                PermissionDecision::ask(format!("{tool_name} needs permission"))
            };
        }

        // 7. Default: read-only auto-approved; writes/other -> tool decision
        //    (which is typically `Ask` and surfaces a prompt, or a Deny).
        if is_read_only {
            PermissionDecision::allow()
        } else {
            tool_decision.clone()
        }
    }

    /// In headless mode an unresolved `Ask` cannot be prompted and must be
    /// treated as a denial (mirrors non-interactive SDK behavior).
    pub fn headless_resolve(&self, decision: PermissionDecision) -> PermissionDecision {
        match decision {
            PermissionDecision::Ask { message } => PermissionDecision::Deny {
                reason: format!(
                    "{message} (auto-denied: no TTY to prompt in --print mode; \
                         use --dangerously-skip-permissions or --allowed-tools)"
                ),
            },
            other => other,
        }
    }
}

/// Match a CLI rule pattern against a tool name. Supports exact names,
/// `Name(...)` spec form (matched on the name prefix), `*` wildcard, and
/// `Name*` prefix wildcards.
pub fn pattern_matches(pattern: &str, tool_name: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }
    // `Tool(spec)` -> match on the `Tool` part.
    let pat_tool = pat.split('(').next().unwrap_or(pat).trim();
    if pat_tool == "*" {
        return true;
    }
    if pat_tool == tool_name {
        return true;
    }
    wildcard_match(pat_tool, tool_name)
}

/// Minimal `*` wildcard matcher. `*` matches any sequence (including empty).
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (n, m) = (p.len(), t.len());

    // dp[i][j]: pattern[..i] matches text[..j]
    let mut dp = vec![vec![false; m + 1]; n + 1];
    dp[0][0] = true;
    for i in 1..=n {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=n {
        for j in 1..=m {
            if p[i - 1] == '*' {
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            } else if p[i - 1] == t[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }
    dp[n][m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard() {
        assert!(wildcard_match("Bash*", "Bash"));
        assert!(wildcard_match("Bash*", "BashTool"));
        assert!(!wildcard_match("Read", "Write"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("File*d", "FileRead"));
    }

    #[test]
    fn bypass_allows_everything() {
        let g = PermissionGate::new(PermissionMode::BypassPermissions, vec![], vec![]);
        let d = g.decide("Bash", false, &PermissionDecision::ask("x"));
        assert!(d.is_allow());
    }

    #[test]
    fn disallowed_wins_over_bypass_pattern_but_not_mode() {
        // disallow list is checked first and wins regardless of mode (except we
        // intentionally still respect bypass AFTER disallow? No: deny wins.)
        let g = PermissionGate::new(PermissionMode::Default, vec![], vec!["Bash".into()]);
        let d = g.decide("Bash", false, &PermissionDecision::allow());
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[test]
    fn default_readonly_auto_allowed() {
        let g = PermissionGate::new(PermissionMode::Default, vec![], vec![]);
        let d = g.decide("Read", true, &PermissionDecision::ask("x"));
        assert!(d.is_allow());
    }

    #[test]
    fn default_write_asks() {
        let g = PermissionGate::new(PermissionMode::Default, vec![], vec![]);
        let d = g.decide("Write", false, &PermissionDecision::ask("x"));
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }

    #[test]
    fn headless_resolves_ask_to_deny() {
        let g = PermissionGate::new(PermissionMode::Default, vec![], vec![]);
        let d = g.headless_resolve(PermissionDecision::ask("need perm"));
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[test]
    fn plan_mode_denies_writes() {
        let g = PermissionGate::new(PermissionMode::Plan, vec![], vec![]);
        assert!(g
            .decide("Read", true, &PermissionDecision::ask("x"))
            .is_allow());
        assert!(matches!(
            g.decide("Write", false, &PermissionDecision::allow()),
            PermissionDecision::Deny { .. }
        ));
    }
}
