//! Plugin hooks — mirror `src/utils/settings/types.ts` hooks schema.
//!
//! Supported hook types (from `HooksSchema` in the TS reference):
//!   PreToolUse, PostToolUse, UserPromptSubmit, SessionStart, SessionEnd,
//!   Stop, SubagentStop, PreCompact, PostCompact
//!
//! Each hook is a shell command; a JSON context object is piped to its stdin.
//! `PreToolUse` can deny a tool call (non-zero exit → Deny); all other types
//! run fire-and-forget / best-effort.

use nonoclaw_core::PermissionDecision;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt;
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

// ---------------------------------------------------------------------------
// Hook type enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookType {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
}

impl fmt::Display for HookType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookType::PreToolUse => f.write_str("PreToolUse"),
            HookType::PostToolUse => f.write_str("PostToolUse"),
            HookType::PostToolUseFailure => f.write_str("PostToolUseFailure"),
            HookType::Notification => f.write_str("Notification"),
            HookType::UserPromptSubmit => f.write_str("UserPromptSubmit"),
            HookType::SessionStart => f.write_str("SessionStart"),
            HookType::SessionEnd => f.write_str("SessionEnd"),
            HookType::Stop => f.write_str("Stop"),
            HookType::SubagentStart => f.write_str("SubagentStart"),
            HookType::SubagentStop => f.write_str("SubagentStop"),
            HookType::PreCompact => f.write_str("PreCompact"),
            HookType::PostCompact => f.write_str("PostCompact"),
        }
    }
}

// ---------------------------------------------------------------------------
// Hook definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    #[serde(default)]
    pub matcher: String,
    /// Shell command (legacy / default hook type).
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Prompt hook: LLM evaluation with small model, JSON schema enforced.
    #[serde(default)]
    pub prompt: Option<PromptHookConfig>,
    /// HTTP hook: POST JSON payload to URL.
    #[serde(default)]
    pub http: Option<HttpHookConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptHookConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpHookConfig {
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// On-disk schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HooksFile {
    #[serde(default)]
    hooks: TypedHooks,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct TypedHooks {
    #[serde(default)]
    pre_tool_use: Vec<HookDef>,
    #[serde(default)]
    post_tool_use: Vec<HookDef>,
    #[serde(default)]
    user_prompt_submit: Vec<HookDef>,
    #[serde(default)]
    session_start: Vec<HookDef>,
    #[serde(default)]
    session_end: Vec<HookDef>,
    #[serde(default)]
    stop: Vec<HookDef>,
    #[serde(default)]
    subagent_stop: Vec<HookDef>,
    #[serde(default)]
    pre_compact: Vec<HookDef>,
    #[serde(default)]
    post_compact: Vec<HookDef>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load all hooks from `.nonoclaw/hooks.json` in cwd.
/// Returns a flat `Vec<(HookType, HookDef)>`.
pub fn load_hooks(cwd: &Path) -> Vec<(HookType, HookDef)> {
    let path = cwd.join(".nonoclaw").join("hooks.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    let Ok(f) = serde_json::from_str::<HooksFile>(&text) else {
        return vec![];
    };
    let h = f.hooks;
    let mut out = Vec::new();
    for d in h.pre_tool_use {
        out.push((HookType::PreToolUse, d));
    }
    for d in h.post_tool_use {
        out.push((HookType::PostToolUse, d));
    }
    for d in h.user_prompt_submit {
        out.push((HookType::UserPromptSubmit, d));
    }
    for d in h.session_start {
        out.push((HookType::SessionStart, d));
    }
    for d in h.session_end {
        out.push((HookType::SessionEnd, d));
    }
    for d in h.stop {
        out.push((HookType::Stop, d));
    }
    for d in h.subagent_stop {
        out.push((HookType::SubagentStop, d));
    }
    for d in h.pre_compact {
        out.push((HookType::PreCompact, d));
    }
    for d in h.post_compact {
        out.push((HookType::PostCompact, d));
    }
    out
}

// ---------------------------------------------------------------------------
// Running helpers
// ---------------------------------------------------------------------------

/// Run all matching hooks of the given type, piping `context` JSON to stdin.
/// Returns fire-and-forget (best-effort, errors logged).
pub async fn run_hooks(
    hooks: &[(HookType, HookDef)],
    ty: HookType,
    tool_name: &str,
    context: &serde_json::Value,
) {
    for (ht, h) in hooks {
        if *ht != ty || !simple_match(&h.matcher, tool_name) {
            continue;
        }
        let payload = serde_json::to_string(context).unwrap_or_default();
        let mut child = match tokio::process::Command::new(&h.command)
            .args(&h.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes()).await;
            drop(stdin);
        }
        let _ = child.wait().await;
    }
}

/// Run matching PreToolUse hooks with a deny path.
pub async fn run_pre_hooks(
    hooks: &[(HookType, HookDef)],
    tool_name: &str,
    context: &serde_json::Value,
) -> PermissionDecision {
    for (ht, h) in hooks {
        if *ht != HookType::PreToolUse || !simple_match(&h.matcher, tool_name) {
            continue;
        }
        let payload = serde_json::to_string(context).unwrap_or_default();
        let result = tokio::process::Command::new(&h.command)
            .args(&h.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match result {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes()).await;
            drop(stdin);
        }
        match child.wait().await {
            Ok(s) if s.code() == Some(0) => continue,
            _ => {
                return PermissionDecision::deny(format!("PreToolUse hook `{}` denied", h.command))
            }
        }
    }
    PermissionDecision::allow()
}

/// Simple glob matcher: `*` suffix wildcards; else exact match.
fn simple_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(p) = pattern.strip_suffix('*') {
        return text.starts_with(p);
    }
    pattern == text
}

// ---------------------------------------------------------------------------
// Context builders
// ---------------------------------------------------------------------------

pub fn tool_context(tool_name: &str, input: &serde_json::Value) -> serde_json::Value {
    json!({
        "tool_name": tool_name,
        "tool_input": input,
        "hook_event_name": "PreToolUse"
    })
}

pub fn prompt_context(prompt: &str) -> serde_json::Value {
    json!({
        "prompt": prompt,
        "hook_event_name": "UserPromptSubmit"
    })
}

pub fn lifecycle_context(event: &str) -> serde_json::Value {
    json!({
        "hook_event_name": event
    })
}

pub fn compact_context(
    removed: usize,
    kept: usize,
    before: usize,
    after: usize,
) -> serde_json::Value {
    json!({
        "removed": removed,
        "kept": kept,
        "tokens_before": before,
        "tokens_after": after,
        "hook_event_name": "PreCompact"
    })
}

pub fn subagent_context(description: &str, result_text: &str) -> serde_json::Value {
    json!({
        "description": description,
        "result": result_text,
        "hook_event_name": "SubagentStop"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher() {
        assert!(simple_match("Bash*", "Bash"));
        assert!(simple_match("*", "anything"));
        assert!(!simple_match("Read", "Write"));
    }

    #[test]
    fn load_parses_all_types() {
        let dir = std::env::temp_dir().join(format!("nc-hooks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".nonoclaw")).unwrap();
        std::fs::write(
            dir.join(".nonoclaw/hooks.json"),
            r#"{
            "hooks": {
                "PreToolUse": [{"command":"true"}],
                "UserPromptSubmit": [{"command":"echo","args":["submit"]}],
                "SessionStart": [],
                "SessionEnd": [{"command":"cleanup"}],
                "Stop": [{"command":"true"}]
            }
        }"#,
        )
        .unwrap();
        let loaded = load_hooks(&dir);
        assert!(loaded.iter().any(|(t, _)| *t == HookType::PreToolUse));
        assert!(loaded.iter().any(|(t, _)| *t == HookType::UserPromptSubmit));
        assert!(loaded.iter().any(|(t, _)| *t == HookType::SessionEnd));
        assert!(loaded.iter().any(|(t, _)| *t == HookType::Stop));
        std::fs::remove_dir_all(&dir).ok();
    }
}
