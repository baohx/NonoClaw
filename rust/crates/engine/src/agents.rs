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
pub fn apply_profile(options: &mut crate::EngineOptions, profile: &AgentProfile) {
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
        // Agent and Coordinator are removed at the depth boundary so direct
        // and batched recursion are blocked. TodoWrite remains available: its
        // canonical store isolates entries by child session scope.
        Ok(registry.filtered(&["Agent", "Coordinator"]))
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
}
