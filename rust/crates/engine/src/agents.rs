//! Agent profile system — pluggable agent definitions from `.nonoclaw/agents/*.md`.
//!
//! Inspired by Grok Build's `AgentDefinition`.  Each profile is a markdown file
//! with YAML frontmatter that overrides system prompt, tool set, and permission
//! mode for a model.  A `models[]` entry references a profile by name via the
//! `profile` field.
//!
//! Files: `<cwd>/.nonoclaw/agents/<name>.md`

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A loaded agent profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentProfile {
    /// Profile name (from `name` frontmatter or filename stem).
    pub name: String,
    /// One-line description.
    #[serde(default)]
    pub description: String,
    /// Additional text appended to the system prompt when this profile is active.
    #[serde(default, rename = "system_prompt_append")]
    pub system_prompt_append: Option<String>,
    /// Tools to allow (if empty, all tools allowed).
    #[serde(default, rename = "tools_allow")]
    pub tools_allow: Vec<String>,
    /// Tools to deny.
    #[serde(default, rename = "tools_deny")]
    pub tools_deny: Vec<String>,
    /// Permission mode override.
    #[serde(default, rename = "permission_mode")]
    pub permission_mode: Option<String>,
    /// Full markdown body (after frontmatter).
    #[serde(default)]
    pub body: String,
}

/// Load an agent profile by name from `<cwd>/.nonoclaw/agents/<name>.md`.
pub fn load_profile(cwd: &Path, name: &str) -> Option<AgentProfile> {
    let path = cwd.join(".nonoclaw/agents").join(format!("{name}.md"));
    load_profile_file(&path)
}

/// Load from an explicit path.
fn load_profile_file(path: &Path) -> Option<AgentProfile> {
    let raw = std::fs::read_to_string(path).ok()?;
    let fm_text = extract_frontmatter(&raw)?;
    let body = strip_frontmatter_text(&raw);
    let mut profile: AgentProfile = serde_yaml::from_str(&fm_text).ok()?;
    if profile.name.is_empty() {
        profile.name = path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("unnamed")
            .to_string();
    }
    profile.body = body;
    Some(profile)
}

/// List all agent profiles in `<cwd>/.nonoclaw/agents/`.
pub fn list_profiles(cwd: &Path) -> Vec<AgentProfile> {
    let dir = cwd.join(".nonoclaw/agents");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out: Vec<AgentProfile> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|p| load_profile_file(&p))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Apply a profile's overrides to [`EngineOptions`].
/// Called after building options but before engine run.
pub fn apply_profile(
    options: &mut crate::EngineOptions,
    profile: &AgentProfile,
) {
    // Merge system prompt appendage.
    if let Some(ref extra) = profile.system_prompt_append {
        let merged = match &options.append_system_prompt {
            Some(existing) => format!("{existing}\n\n{extra}"),
            None => extra.clone(),
        };
        options.append_system_prompt = Some(merged);
    }
    // Override allowed/disallowed tools.
    if !profile.tools_allow.is_empty() {
        options.allowed_tools = profile.tools_allow.clone();
    }
    if !profile.tools_deny.is_empty() {
        options.disallowed_tools = profile.tools_deny.clone();
    }
    // Override permission mode.
    if let Some(ref mode) = profile.permission_mode {
        if let Some(m) = nonoclaw_core::PermissionMode::from_kebab(mode) {
            options.permission_mode = m;
        }
    }
}

/// Extract YAML frontmatter text between `---` delimiters.
fn extract_frontmatter(raw: &str) -> Option<String> {
    let s = raw.trim();
    if !s.starts_with("---") { return None; }
    let after = &s[3..];
    let end = after.find("\n---")?;
    Some(after[..end].to_string())
}

/// Strip YAML frontmatter, returning body text.
fn strip_frontmatter_text(raw: &str) -> String {
    let s = raw.trim();
    if !s.starts_with("---") { return s.to_string(); }
    let after = &s[3..];
    if let Some(pos) = after.find("\n---") {
        after[pos + 4..].trim().to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_profile() {
        let md = r#"---
name: test-agent
description: A test
system_prompt_append: "Be careful."
tools_allow: [Read, Write]
permission_mode: plan
---
# Test Agent
Body text here."#;
        let fm = extract_frontmatter(md).unwrap();
        let profile: AgentProfile = serde_yaml::from_str(&fm).unwrap();
        assert_eq!(profile.name, "test-agent");
        assert_eq!(profile.tools_allow, vec!["Read", "Write"]);
        assert_eq!(profile.permission_mode.as_deref(), Some("plan"));
        let body = strip_frontmatter_text(md);
        assert!(body.contains("Body text here"));
    }

    #[test]
    fn apply_profile_overrides() {
        let profile = AgentProfile {
            name: "test".into(),
            system_prompt_append: Some("Extra instructions.".into()),
            tools_allow: vec!["Read".into(), "Write".into()],
            tools_deny: vec!["Bash".into()],
            permission_mode: Some("acceptEdits".into()),
            ..Default::default()
        };
        let mut opts = crate::EngineOptions::default();
        opts.append_system_prompt = Some("Base prompt.".into());
        opts.allowed_tools = vec!["Bash".into()];
        apply_profile(&mut opts, &profile);
        assert!(opts.append_system_prompt.unwrap().contains("Extra instructions"));
        assert_eq!(opts.allowed_tools, vec!["Read", "Write"]);
        assert_eq!(opts.disallowed_tools, vec!["Bash"]);
    }
}
