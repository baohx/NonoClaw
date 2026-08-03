//! Agent profile system — pluggable agent definitions from `.nonoclaw/agents/*.md`.
//!
//! Inspired by Grok Build's `AgentDefinition`.  Each profile is a markdown file
//! with YAML frontmatter that overrides system prompt, tool set, and permission
//! mode for a model.  A `models[]` entry references a profile by name via the
//! `profile` field.
//!
//! Files: `<cwd>/.nonoclaw/agents/<name>.md`

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use nonoclaw_core::{Error, Result};
use nonoclaw_tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

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
    /// When set, **completely replaces** the fixed subagent prompt (and any
    /// append text) with this content. Use for highly specialized agents that
    /// need an entirely different instruction set rather than an addition.
    #[serde(default, rename = "system_prompt_override")]
    pub system_prompt_override: Option<String>,
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

const MAX_PROFILE_BYTES: u64 = 64 * 1024;
const MAX_PROFILE_NAME_CHARS: usize = 128;

/// Strictly load an agent profile by safe basename from
/// `<cwd>/.nonoclaw/agents/<name>.md`.
pub fn load_profile_checked(cwd: &Path, name: &str) -> Result<AgentProfile> {
    validate_profile_name(name)?;
    let path = cwd.join(".nonoclaw/agents").join(format!("{name}.md"));
    let metadata = std::fs::metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::Config(format!("agent profile `{name}` was not found"))
        } else {
            Error::Config(format!("failed to inspect agent profile `{name}`: {error}"))
        }
    })?;
    if !metadata.is_file() {
        return Err(Error::Config(format!(
            "agent profile `{name}` is not a regular file"
        )));
    }
    if metadata.len() > MAX_PROFILE_BYTES {
        return Err(Error::Config(format!(
            "agent profile `{name}` exceeds the 64 KiB size limit"
        )));
    }
    let bytes = std::fs::read(&path).map_err(|error| {
        Error::Config(format!("failed to read agent profile `{name}`: {error}"))
    })?;
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err(Error::Config(format!(
            "agent profile `{name}` exceeds the 64 KiB size limit"
        )));
    }
    let raw = String::from_utf8(bytes)
        .map_err(|_| Error::Config(format!("agent profile `{name}` is not valid UTF-8")))?;
    let fm_text = extract_frontmatter(&raw).ok_or_else(|| {
        Error::Config(format!(
            "agent profile `{name}` must contain YAML frontmatter"
        ))
    })?;
    let body = strip_frontmatter_text(&raw);
    let mut profile: AgentProfile = serde_yaml::from_str(&fm_text).map_err(|error| {
        Error::Config(format!(
            "failed to parse agent profile `{name}` frontmatter: {error}"
        ))
    })?;
    if profile.name.trim().is_empty() {
        profile.name = name.to_string();
    }
    if let Some(mode) = profile.permission_mode.as_deref() {
        if nonoclaw_core::PermissionMode::from_kebab(mode).is_none() {
            return Err(Error::Config(format!(
                "agent profile `{name}` has invalid permission mode `{mode}`"
            )));
        }
    }
    profile.body = body;
    Ok(profile)
}

fn validate_profile_name(name: &str) -> Result<()> {
    let safe = !name.is_empty()
        && name.chars().count() <= MAX_PROFILE_NAME_CHARS
        && !name.starts_with('.')
        && !name.contains("..")
        && !name.contains(['/', '\\'])
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if safe {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "invalid agent profile name `{name}`: expected a safe non-hidden basename"
        )))
    }
}

/// Backward-compatible optional loader used by extension discovery. Child
/// Agent execution uses [`load_profile_checked`] so failures are never ignored.
pub fn load_profile(cwd: &Path, name: &str) -> Option<AgentProfile> {
    load_profile_checked(cwd, name).ok()
}

/// Load from an explicit path for best-effort profile listing.
fn load_profile_file(path: &Path) -> Option<AgentProfile> {
    let raw = std::fs::read_to_string(path).ok()?;
    if raw.len() as u64 > MAX_PROFILE_BYTES {
        return None;
    }
    let fm_text = extract_frontmatter(&raw)?;
    let body = strip_frontmatter_text(&raw);
    let mut profile: AgentProfile = serde_yaml::from_str(&fm_text).ok()?;
    if profile
        .permission_mode
        .as_deref()
        .is_some_and(|mode| nonoclaw_core::PermissionMode::from_kebab(mode).is_none())
    {
        return None;
    }
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
pub fn apply_profile(options: &mut crate::EngineOptions, profile: &AgentProfile) {
    if let Some(ref extra) = profile.system_prompt_append {
        let merged = match &options.append_system_prompt {
            Some(existing) => format!("{existing}\n\n{extra}"),
            None => extra.clone(),
        };
        options.append_system_prompt = Some(merged);
    }
    if !profile.tools_allow.is_empty() {
        options.allowed_tools = profile.tools_allow.clone();
    }
    if !profile.tools_deny.is_empty() {
        options.disallowed_tools = profile.tools_deny.clone();
    }
    if let Some(ref mode) = profile.permission_mode {
        if let Some(mode) = nonoclaw_core::PermissionMode::from_kebab(mode) {
            options.permission_mode = mode;
        }
    }
}

/// Apply a profile to child options without ever broadening the parent's
/// capabilities. The fixed autonomous-child prompt remains authoritative and
/// is followed only by the profile's explicit prompt appendage.
pub(crate) fn apply_subagent_profile(
    options: &mut crate::EngineOptions,
    profile: Option<&AgentProfile>,
    fixed_prompt: String,
) {
    if let Some(profile) = profile {
        // tools_allow is enforced as registry visibility by EngineSubagent.
        // Keep the parent's explicit permission allow rules unchanged: adding
        // a profile tool here would bypass Plan/default headless protections.
        for denied in &profile.tools_deny {
            if !options.disallowed_tools.contains(denied) {
                options.disallowed_tools.push(denied.clone());
            }
        }
        if let Some(mode) = profile
            .permission_mode
            .as_deref()
            .and_then(nonoclaw_core::PermissionMode::from_kebab)
        {
            if permission_strictness(mode) <= permission_strictness(options.permission_mode) {
                options.permission_mode = mode;
            }
        }
    }

    // Override takes complete precedence over append: the profile provides
    // a standalone instruction set, bypassing the default fixed prompt.
    options.append_system_prompt = Some(match profile {
        Some(p) if p.system_prompt_override.as_deref().is_some_and(|s| !s.trim().is_empty()) => {
            p.system_prompt_override.clone().unwrap()
        }
        _ => match profile.and_then(|p| p.system_prompt_append.as_deref()) {
            Some(extra) if !extra.trim().is_empty() => format!("{fixed_prompt}\n\n{extra}"),
            _ => fixed_prompt,
        },
    });
}

fn permission_strictness(mode: nonoclaw_core::PermissionMode) -> u8 {
    match mode {
        nonoclaw_core::PermissionMode::Plan => 0,
        nonoclaw_core::PermissionMode::Default => 1,
        nonoclaw_core::PermissionMode::AcceptEdits => 2,
        nonoclaw_core::PermissionMode::Auto => 3,
        nonoclaw_core::PermissionMode::BypassPermissions => 4,
    }
}

/// Maximum recursive delegation depth. A root agent is depth zero and may
/// create one child level; child registries no longer expose Agent/Coordinator.
const MAX_SUBAGENT_DEPTH: usize = 1;
const DEFAULT_MAX_SUBAGENT_CONCURRENCY: usize = 4;
const MAX_SUBAGENT_CONCURRENCY: usize = 64;

/// Canonical owner for subagent recursion, tool filtering, concurrency, and
/// cancellation policy. Agent and Coordinator both execute through this gate.
#[derive(Clone)]
pub(crate) struct SubagentLifecycle {
    depth: usize,
    max_depth: usize,
    semaphore: Arc<Semaphore>,
    cancel: CancellationToken,
}

impl SubagentLifecycle {
    pub(crate) fn new(cancel: CancellationToken) -> Self {
        Self::with_limits(
            cancel,
            0,
            MAX_SUBAGENT_DEPTH,
            max_subagent_concurrency_from_env(),
        )
    }

    fn with_limits(
        cancel: CancellationToken,
        depth: usize,
        max_depth: usize,
        max_concurrency: usize,
    ) -> Self {
        Self {
            depth,
            max_depth,
            semaphore: Arc::new(Semaphore::new(max_concurrency.max(1))),
            cancel,
        }
    }

    pub(crate) fn child_registry(&self, registry: &ToolRegistry) -> Result<ToolRegistry> {
        if self.depth >= self.max_depth {
            return Err(Error::Other(format!(
                "subagent recursion depth {} reached the limit {}",
                self.depth, self.max_depth
            )));
        }
        // Agent, Coordinator, and Graph are removed at the depth boundary so
        // direct, batched, and graph-based recursion are all blocked. TodoWrite
        // remains available: its canonical store isolates entries by child
        // session scope.
        Ok(registry.filtered(&["Agent", "Coordinator", "Graph"]))
    }

    /// Cancellation token shared by every child spawned under this lifecycle.
    pub(crate) fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Current recursion depth (0 for a root agent).
    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    /// Maximum allowed recursion depth.
    pub(crate) fn max_depth(&self) -> usize {
        self.max_depth
    }

    pub(crate) async fn run<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        if self.depth >= self.max_depth {
            return Err(Error::Other(format!(
                "subagent recursion depth {} reached the limit {}",
                self.depth, self.max_depth
            )));
        }
        let permit = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(Error::Cancelled),
            permit = Arc::clone(&self.semaphore).acquire_owned() => {
                permit.map_err(|_| Error::Cancelled)?
            }
        };
        self.run_with_permit(future, permit).await
    }

    async fn run_with_permit<T>(
        &self,
        future: impl Future<Output = Result<T>>,
        _permit: OwnedSemaphorePermit,
    ) -> Result<T> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(Error::Cancelled),
            result = future => result,
        }
    }
}

fn max_subagent_concurrency_from_env() -> usize {
    std::env::var("NONOCLAW_MAX_SUBAGENT_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_SUBAGENT_CONCURRENCY)
        .min(MAX_SUBAGENT_CONCURRENCY)
}

/// Extract YAML frontmatter text between `---` delimiters.
fn extract_frontmatter(raw: &str) -> Option<String> {
    let s = raw.trim();
    if !s.starts_with("---") {
        return None;
    }
    let after = &s[3..];
    let end = after.find("\n---")?;
    Some(after[..end].to_string())
}

/// Strip YAML frontmatter, returning body text.
fn strip_frontmatter_text(raw: &str) -> String {
    let s = raw.trim();
    if !s.starts_with("---") {
        return s.to_string();
    }
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
        let mut opts = crate::EngineOptions {
            append_system_prompt: Some("Base prompt.".into()),
            allowed_tools: vec!["Bash".into()],
            ..Default::default()
        };
        apply_profile(&mut opts, &profile);
        assert!(opts
            .append_system_prompt
            .unwrap()
            .contains("Extra instructions"));
        assert_eq!(opts.allowed_tools, vec!["Read", "Write"]);
        assert_eq!(opts.disallowed_tools, vec!["Bash"]);
    }

    fn profile_test_dir() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nonoclaw-agent-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".nonoclaw/agents")).unwrap();
        dir
    }

    #[test]
    fn checked_profile_rejects_unsafe_missing_and_invalid_mode() {
        let cwd = profile_test_dir();
        for name in [
            "",
            ".hidden",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            "white space",
        ] {
            assert!(
                matches!(load_profile_checked(&cwd, name), Err(Error::Config(message)) if message.contains("invalid agent profile name"))
            );
        }
        assert!(matches!(
            load_profile_checked(&cwd, "missing"),
            Err(Error::Config(message)) if message.contains("was not found")
        ));
        std::fs::write(
            cwd.join(".nonoclaw/agents/bad-mode.md"),
            "---\nname: bad-mode\npermission_mode: superuser\n---\nbody\n",
        )
        .unwrap();
        assert!(matches!(
            load_profile_checked(&cwd, "bad-mode"),
            Err(Error::Config(message)) if message.contains("invalid permission mode")
        ));
        std::fs::remove_dir_all(cwd).ok();
    }

    #[test]
    fn subagent_profile_merges_prompt_and_only_tightens_permissions() {
        let profile = AgentProfile {
            name: "restricted".into(),
            system_prompt_append: Some("Profile-only guidance.".into()),
            tools_allow: vec!["Read".into(), "Bash".into()],
            tools_deny: vec!["Bash".into(), "Write".into()],
            permission_mode: Some("plan".into()),
            ..Default::default()
        };
        let mut options = crate::EngineOptions {
            append_system_prompt: Some("parent prompt must not replace child prompt".into()),
            allowed_tools: vec!["Read".into(), "Write".into()],
            disallowed_tools: vec!["Grep".into()],
            permission_mode: nonoclaw_core::PermissionMode::Default,
            ..Default::default()
        };
        apply_subagent_profile(&mut options, Some(&profile), "Fixed child prompt.".into());
        assert_eq!(options.allowed_tools, vec!["Read", "Write"]);
        assert_eq!(options.disallowed_tools, vec!["Grep", "Bash", "Write"]);
        assert_eq!(options.permission_mode, nonoclaw_core::PermissionMode::Plan);
        assert_eq!(
            options.append_system_prompt.as_deref(),
            Some("Fixed child prompt.\n\nProfile-only guidance.")
        );

        let less_strict = AgentProfile {
            permission_mode: Some("auto".into()),
            tools_allow: vec!["Bash".into()],
            ..Default::default()
        };
        apply_subagent_profile(&mut options, Some(&less_strict), "Fixed.".into());
        assert_eq!(options.permission_mode, nonoclaw_core::PermissionMode::Plan);
        assert_eq!(options.allowed_tools, vec!["Read", "Write"]);
    }

    #[test]
    fn subagent_profile_allow_is_visibility_only_and_never_grants_permission() {
        let profile = AgentProfile {
            name: "writer".into(),
            tools_allow: vec!["Write".into(), "Bash".into()],
            ..Default::default()
        };

        for mode in [
            nonoclaw_core::PermissionMode::Plan,
            nonoclaw_core::PermissionMode::Default,
        ] {
            let mut options = crate::EngineOptions {
                permission_mode: mode,
                allowed_tools: Vec::new(),
                ..Default::default()
            };
            apply_subagent_profile(&mut options, Some(&profile), "Fixed.".into());
            assert!(options.allowed_tools.is_empty());

            let gate = nonoclaw_tools::PermissionGate::new(
                options.permission_mode,
                options.allowed_tools.clone(),
                options.disallowed_tools.clone(),
            );
            let decision = gate.decide(
                "Write",
                false,
                &nonoclaw_core::PermissionDecision::ask("write requires permission"),
            );
            let resolved = gate.headless_resolve(decision);
            assert!(matches!(
                resolved,
                nonoclaw_core::PermissionDecision::Deny { .. }
            ));
        }

        let (registry, _) = nonoclaw_tools::register_all();
        let visible = registry
            .filtered(&["Agent", "Coordinator"])
            .restricted_to(&profile.tools_allow);
        assert!(visible.find("Write").is_some());
        assert!(visible.find("Bash").is_some());
        assert!(visible.find("Read").is_none());
        assert!(visible.find("Agent").is_none());
    }

    #[tokio::test]
    async fn multiple_subagents_run_in_parallel_with_a_hard_cap() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let lifecycle = SubagentLifecycle::with_limits(CancellationToken::new(), 0, 1, 2);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let futures = (0..6).map(|index| {
            let lifecycle = lifecycle.clone();
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            async move {
                lifecycle
                    .run(async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(index)
                    })
                    .await
            }
        });
        let results = futures::future::join_all(futures).await;
        assert_eq!(
            results.into_iter().collect::<Result<Vec<_>>>().unwrap(),
            (0..6).collect::<Vec<_>>()
        );
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn subagent_partial_failure_does_not_cancel_siblings() {
        let lifecycle = SubagentLifecycle::with_limits(CancellationToken::new(), 0, 1, 3);
        let futures = (0..3).map(|index| {
            let lifecycle = lifecycle.clone();
            async move {
                lifecycle
                    .run(async move {
                        if index == 1 {
                            Err(Error::Other("fixture failure".into()))
                        } else {
                            Ok(format!("result-{index}"))
                        }
                    })
                    .await
            }
        });
        let results = futures::future::join_all(futures).await;
        assert_eq!(results[0].as_deref().unwrap(), "result-0");
        assert!(matches!(&results[1], Err(Error::Other(message)) if message == "fixture failure"));
        assert_eq!(results[2].as_deref().unwrap(), "result-2");
    }

    #[tokio::test]
    async fn parent_cancellation_stops_all_subagents() {
        use std::time::Duration;

        let cancel = CancellationToken::new();
        let lifecycle = SubagentLifecycle::with_limits(cancel.clone(), 0, 1, 4);
        let tasks = (0..4).map(|_| {
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move {
                lifecycle
                    .run(async {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        Ok(())
                    })
                    .await
            })
        });
        let handles = tasks.collect::<Vec<_>>();
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        for handle in handles {
            let result = tokio::time::timeout(Duration::from_secs(1), handle)
                .await
                .expect("cancelled subagent must stop promptly")
                .unwrap();
            assert!(matches!(result, Err(Error::Cancelled)));
        }
    }

    #[tokio::test]
    async fn recursion_is_filtered_and_child_todos_remain_isolated_by_scope() {
        let (registry, _) = nonoclaw_tools::register_all();
        let lifecycle = SubagentLifecycle::with_limits(CancellationToken::new(), 0, 1, 1);
        let child = lifecycle.child_registry(&registry).unwrap();
        assert!(child.find("Agent").is_none());
        assert!(child.find("Coordinator").is_none());
        assert!(child.find("TodoWrite").is_some());
        assert!(child.find("TaskCreate").is_some());
        assert!(child.find("Read").is_some());

        let at_limit = SubagentLifecycle::with_limits(CancellationToken::new(), 1, 1, 1);
        assert!(matches!(
            at_limit.run(async { Ok(()) }).await,
            Err(Error::Other(message)) if message.contains("recursion depth")
        ));
    }

    #[test]
    fn system_prompt_override_replaces_fixed_prompt_entirely() {
        let profile = AgentProfile {
            name: "specialist".into(),
            system_prompt_override: Some(
                "You are a code reviewer. Review code diffs and provide feedback only.".into(),
            ),
            ..Default::default()
        };
        let mut options = crate::EngineOptions::default();
        apply_subagent_profile(&mut options, Some(&profile), "Default fixed prompt.".into());
        assert_eq!(
            options.append_system_prompt.as_deref(),
            Some("You are a code reviewer. Review code diffs and provide feedback only.")
        );
        assert!(!options
            .append_system_prompt
            .as_deref()
            .unwrap_or("")
            .contains("Default fixed prompt."));
    }

    #[test]
    fn system_prompt_override_takes_precedence_over_append() {
        let profile = AgentProfile {
            name: "specialist".into(),
            system_prompt_override: Some("Override content.".into()),
            system_prompt_append: Some("Append content.".into()),
            ..Default::default()
        };
        let mut options = crate::EngineOptions::default();
        apply_subagent_profile(&mut options, Some(&profile), "Fixed.".into());
        assert_eq!(options.append_system_prompt.as_deref(), Some("Override content."));
    }

    #[test]
    fn empty_system_prompt_override_falls_back_to_append() {
        let profile = AgentProfile {
            name: "specialist".into(),
            system_prompt_override: Some("   ".into()),
            system_prompt_append: Some("Append content.".into()),
            ..Default::default()
        };
        let mut options = crate::EngineOptions::default();
        apply_subagent_profile(&mut options, Some(&profile), "Fixed prompt.".into());
        assert_eq!(
            options.append_system_prompt.as_deref(),
            Some("Fixed prompt.\n\nAppend content.")
        );
    }
}
