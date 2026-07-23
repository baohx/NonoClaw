//! Project-context gatherer for the web UI "Insight" rail + Git pane.
//!
//! Collects, in one bounded pass, everything worth showing about the current
//! project: the registered tools (built-in + MCP), MCP servers, skills,
//! plugins, the layered NONOCLAW.md / settings files, and a structured git
//! snapshot. Sent to the browser as `ServerMsg::ProjectInfo`.

use std::path::{Path, PathBuf};

use clap::CommandFactory;
use nonoclaw_core::redact_text;
use nonoclaw_engine::skills::Skill;
use nonoclaw_engine::{
    ConfigDiagnostic, ConfigFieldReference, ExtensionDescriptor, ExtensionDiagnostic,
    ExtensionKind, ResolvedConfig,
};
use nonoclaw_tools::ToolRegistry;
use serde::Serialize;

/// The full project-context payload sent to the browser.
#[derive(Debug, Serialize, Clone)]
pub struct ProjectInfo {
    pub cwd: String,
    pub model: String,
    pub tools: Vec<ToolInfo>,
    pub mcp_servers: Vec<McpServerInfo>,
    pub skills: Vec<SkillInfo>,
    pub plugins: Vec<PluginInfo>,
    /// Shared source/status records for Skills, Profiles, Plugins, and MCP.
    pub extensions: Vec<ExtensionDescriptor>,
    /// Non-fatal load and deterministic name-conflict diagnostics.
    pub extension_diagnostics: Vec<ExtensionDiagnostic>,
    pub hooks: Vec<HookEntry>,
    pub docs: Vec<PathLayer>,
    pub settings: Vec<PathLayer>,
    /// Generated from the Clap command definition used by the executable.
    pub cli_reference: Vec<ReferenceItem>,
    /// Shared top-level settings metadata also used by unknown-field diagnostics.
    pub config_reference: Vec<ConfigFieldReference>,
    /// Safe field/file-level configuration diagnostics for the Insight rail.
    pub config_diagnostics: Vec<ConfigDiagnosticInfo>,
    pub git: Option<GitInfo>,
    /// Configured model context window (tokens), if set.
    pub context_window: Option<usize>,
    /// Effective auto-compact threshold (tokens).
    pub compact_threshold: usize,
    /// Public URL for QR-code mobile access, if set via --public-url.
    pub public_url: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub kind: String, // "builtin" | "mcp"
    pub mcp_server: Option<String>,
    pub read_only: bool,
    pub aliases: Vec<String>,
    pub prompt_preview: String, // ≤400 chars
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Serialize, Clone)]
pub struct McpServerInfo {
    pub name: String,
    pub command: String,
    pub config_source: Option<String>,
    pub connected: bool,
    pub tool_count: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub source: String, // on-disk SKILL.md path
    pub body: String,   // full markdown body (injected as append_system_prompt)
}

#[derive(Debug, Serialize, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub dir: String, // abs install dir
    pub skill_count: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct HookEntry {
    pub hook_type: String,
    pub matcher: String,
    pub command: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct PathLayer {
    pub label: String,
    pub path: String, // absolute, for click-to-open
    pub exists: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct ReferenceItem {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ConfigDiagnosticInfo {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub field: Option<String>,
    pub source: Option<String>,
    pub related_source: Option<String>,
    pub suggestion: String,
}

impl From<&ConfigDiagnostic> for ConfigDiagnosticInfo {
    fn from(diagnostic: &ConfigDiagnostic) -> Self {
        Self {
            severity: format!("{:?}", diagnostic.severity).to_lowercase(),
            code: diagnostic.code.clone(),
            message: diagnostic.message.clone(),
            field: diagnostic.field.clone(),
            source: diagnostic.source.as_ref().map(|source| source.label()),
            related_source: diagnostic
                .related_source
                .as_ref()
                .map(|source| source.label()),
            suggestion: diagnostic.suggestion.clone(),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct GitInfo {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
    pub conflicts: u32,
    pub is_empty: bool,
    pub recent_commits: Vec<CommitInfo>,
    pub user: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

/// The nonoclaw home dir: `$NONOCLAW_HOME` or `~/.nonoclaw`.
pub fn nonoclaw_home() -> Option<PathBuf> {
    nonoclaw_core::nonoclaw_data_dir()
}

/// Gather the full project context. All local FS probes + a handful of git
/// subprocess calls; bounded (commits ≤10, prompt_preview ≤400 chars).
#[allow(clippy::too_many_arguments)]
pub async fn gather(
    cwd: &Path,
    model: &str,
    registry: &ToolRegistry,
    config: &ResolvedConfig,
    public_url: Option<String>,
    skills: &[Skill],
    skill_extensions: &[ExtensionDescriptor],
    skill_diagnostics: &[ExtensionDiagnostic],
) -> ProjectInfo {
    let tools: Vec<ToolInfo> = registry
        .all()
        .iter()
        .map(|t| {
            let name = t.name().to_string();
            let (kind, mcp_server) = if let Some(rest) = name.strip_prefix("mcp__") {
                let server = rest.split("__").next().unwrap_or("").to_string();
                ("mcp".to_string(), Some(server))
            } else {
                ("builtin".to_string(), None)
            };
            ToolInfo {
                name,
                description: t.description().to_string(),
                kind,
                mcp_server: mcp_server.clone(),
                read_only: t.is_read_only(&serde_json::json!({})),
                aliases: t.aliases().iter().map(|s| s.to_string()).collect(),
                prompt_preview: "[tool prompt hidden]".into(),
                input_schema: t.input_schema(),
            }
        })
        .collect();

    // MCP servers: derive connected/tool_count from what actually registered.
    let mcp_configs = config.mcp_configs();
    let mcp_sources = config.mcp_source_labels();
    let mcp_servers = mcp_configs
        .iter()
        .map(|(name, cfg)| {
            let tool_count = tools
                .iter()
                .filter(|t| t.mcp_server.as_deref() == Some(name.as_str()))
                .count();
            McpServerInfo {
                name: name.clone(),
                command: command_source(&cfg.command, cfg.args.len()),
                config_source: mcp_sources.get(name).cloned(),
                connected: tool_count > 0,
                tool_count,
            }
        })
        .collect();

    let skills: Vec<SkillInfo> = skills
        .iter()
        .map(|s| SkillInfo {
            name: s.name.clone(),
            description: s.description.clone(),
            source: s.source.clone(),
            body: "[skill content kept server-side]".into(),
        })
        .collect();

    let plugins = list_plugins(cwd);
    let mut discovery = nonoclaw_engine::extensions::discover_profiles(cwd);
    discovery.merge(nonoclaw_engine::extensions::discover_plugins(cwd));
    discovery
        .descriptors
        .extend(skill_extensions.iter().cloned());
    discovery
        .descriptors
        .extend(registry.extension_descriptors().iter().cloned());
    discovery
        .diagnostics
        .extend(skill_diagnostics.iter().cloned());
    discovery
        .diagnostics
        .extend(registry.extension_diagnostics().iter().cloned());
    // MCP source labels come from the canonical resolved configuration rather
    // than command strings, so Insight never needs to infer provenance.
    for descriptor in &mut discovery.descriptors {
        if descriptor.kind == ExtensionKind::Mcp {
            if let Some(source) = mcp_sources.get(&descriptor.name) {
                descriptor.source = source.clone();
            }
        }
        descriptor.detail = descriptor.detail.as_deref().map(redact_text);
    }
    for diagnostic in &mut discovery.diagnostics {
        diagnostic.message = redact_text(&diagnostic.message);
        diagnostic.suggestion = redact_text(&diagnostic.suggestion);
    }
    discovery.descriptors.sort_by(|a, b| {
        a.kind
            .as_str()
            .cmp(b.kind.as_str())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| b.precedence.cmp(&a.precedence))
    });
    let hooks = nonoclaw_engine::hooks::load_hooks(cwd)
        .into_iter()
        .map(|(t, d)| HookEntry {
            hook_type: t.to_string(),
            matcher: d.matcher.clone(),
            command: hook_source(&d),
        })
        .collect();

    let docs = collect_doc_layers(cwd);
    let settings = collect_settings_layers(cwd);
    let config_diagnostics = config
        .diagnostics
        .iter()
        .map(ConfigDiagnosticInfo::from)
        .collect();
    let (context_window, compact_threshold) = config.model_budget(model);
    let git = git_info(cwd).await;

    ProjectInfo {
        cwd: cwd.to_string_lossy().to_string(),
        model: model.to_string(),
        tools,
        mcp_servers,
        skills,
        plugins,
        extensions: discovery.descriptors,
        extension_diagnostics: discovery.diagnostics,
        hooks,
        docs,
        settings,
        cli_reference: cli_reference(),
        config_reference: nonoclaw_engine::config_reference().to_vec(),
        config_diagnostics,
        git,
        context_window,
        compact_threshold,
        public_url,
    }
}

fn cli_reference() -> Vec<ReferenceItem> {
    crate::Cli::command()
        .get_arguments()
        .filter(|argument| !argument.is_positional())
        .filter_map(|argument| {
            let long = argument.get_long()?;
            let mut name = argument
                .get_short()
                .map(|short| format!("-{short}, --{long}"))
                .unwrap_or_else(|| format!("--{long}"));
            if argument.get_action().takes_values() {
                let value_name = argument
                    .get_value_names()
                    .and_then(|names| names.first())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "VALUE".into());
                name.push_str(&format!(" <{value_name}>"));
            }
            Some(ReferenceItem {
                name,
                description: argument
                    .get_help()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn command_source(command: &str, argument_count: usize) -> String {
    let executable = (!command.contains("://") && !command.contains('?') && !command.contains('#'))
        .then(|| {
            Path::new(command)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= 128
                        && name.chars().all(|character| {
                            character.is_ascii_alphanumeric() || "-_.".contains(character)
                        })
                })
        })
        .flatten()
        .unwrap_or("configured command");
    format!("{executable} ({argument_count} argument(s), values hidden)")
}

fn hook_source(definition: &nonoclaw_engine::hooks::HookDef) -> String {
    if !definition.command.trim().is_empty() {
        return format!(
            "command · {}",
            command_source(&definition.command, definition.args.len())
        );
    }
    if let Some(prompt) = &definition.prompt {
        return format!(
            "prompt · {}",
            prompt.model.as_deref().unwrap_or("default model")
        );
    }
    if let Some(http) = &definition.http {
        let origin = reqwest::Url::parse(&http.url)
            .ok()
            .and_then(|url| {
                url.host_str()
                    .map(|host| format!("{}://{host}", url.scheme()))
            })
            .unwrap_or_else(|| "configured endpoint".into());
        return format!("http · {origin} (path, query, and headers hidden)");
    }
    "invalid hook action".into()
}

/// Scan plugin directories for contributed skills.
/// Checks both `~/.nonoclaw/plugins/` (user-global) and
/// `<cwd>/.nonoclaw/plugins/` (project).
fn list_plugins(cwd: &Path) -> Vec<PluginInfo> {
    let mut out = Vec::new();
    let mut dirs_to_scan: Vec<PathBuf> = Vec::new();

    // User-global
    if let Some(home) = nonoclaw_home() {
        dirs_to_scan.push(home.join("plugins"));
    }
    // Project
    dirs_to_scan.push(cwd.join(".nonoclaw").join("plugins"));

    for plugins_dir in &dirs_to_scan {
        let Ok(entries) = std::fs::read_dir(plugins_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Count <plugin>/skills/<skill>/SKILL.md
            let skill_count = std::fs::read_dir(path.join("skills"))
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.path().join("SKILL.md").exists())
                        .count()
                })
                .unwrap_or(0);
            out.push(PluginInfo {
                name,
                dir: path.to_string_lossy().to_string(),
                skill_count,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The NONOCLAW.md / rules / MEMORY.md files the engine actually loads, plus
/// the repo-root NONOCLAW.md (which the engine does NOT auto-read — labelled so).
fn collect_doc_layers(cwd: &Path) -> Vec<PathLayer> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<PathLayer>, label: &str, path: PathBuf| {
        out.push(PathLayer {
            label: label.into(),
            exists: path.exists(),
            path: path.to_string_lossy().to_string(),
        });
    };

    push(
        &mut out,
        "Project · NONOCLAW.md",
        cwd.join(".nonoclaw/NONOCLAW.md"),
    );
    push(
        &mut out,
        "Project · NONOCLAW.local.md",
        cwd.join(".nonoclaw/NONOCLAW.local.md"),
    );
    push(
        &mut out,
        "Project · memory/MEMORY.md",
        cwd.join(".nonoclaw/memory/MEMORY.md"),
    );
    // Project rules/*.md
    extend_rules(&mut out, "Project · rules", &cwd.join(".nonoclaw/rules"));

    if let Some(home) = nonoclaw_home() {
        push(&mut out, "User · NONOCLAW.md", home.join("NONOCLAW.md"));
        extend_rules(&mut out, "User · rules", &home.join("rules"));
    }

    // Repo-root docs — the engine does NOT auto-read these; shown for awareness.
    push(
        &mut out,
        "Repo root · NONOCLAW.md (not auto-loaded)",
        cwd.join("NONOCLAW.md"),
    );
    if cwd.join("NONOCLAW.zh-CN.md").exists() {
        push(
            &mut out,
            "Repo root · NONOCLAW.zh-CN.md (not auto-loaded)",
            cwd.join("NONOCLAW.zh-CN.md"),
        );
    }
    out
}

fn extend_rules(out: &mut Vec<PathLayer>, prefix: &str, rules_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(rules_dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();
    for p in paths {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("rule.md");
        out.push(PathLayer {
            label: format!("{prefix}/{name}"),
            exists: true,
            path: p.to_string_lossy().to_string(),
        });
    }
}

/// The layered settings files (precedence low→high) + the MCP config file.
fn collect_settings_layers(cwd: &Path) -> Vec<PathLayer> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<PathLayer>, label: &str, path: PathBuf| {
        out.push(PathLayer {
            label: label.into(),
            exists: path.exists(),
            path: path.to_string_lossy().to_string(),
        });
    };
    if let Some(home) = nonoclaw_home() {
        push(&mut out, "User · settings.json", home.join("settings.json"));
    }
    push(
        &mut out,
        "Project · settings.json",
        cwd.join(".nonoclaw/settings.json"),
    );
    push(
        &mut out,
        "Project · settings.local.json",
        cwd.join(".nonoclaw/settings.local.json"),
    );
    push(
        &mut out,
        "Project · mcp.json",
        cwd.join(".nonoclaw/mcp.json"),
    );
    push(
        &mut out,
        "Project · hooks.json",
        cwd.join(".nonoclaw/hooks.json"),
    );
    out
}

/// Structured git snapshot. Returns `None` outside a repo. Shells out (no git2
/// dep) — mirrors `engine::context::git_out`.
async fn git_info(cwd: &Path) -> Option<GitInfo> {
    let branch_raw = git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    // Empty repo (git init, no commits): rev-parse fails but .git/ exists.
    // Return is_empty=true instead of None so the UI shows "no commits yet".
    if branch_raw.is_empty() {
        if !cwd.join(".git").exists() {
            return None; // genuinely not a git repo
        }
        let user_raw = git_out(cwd, &["config", "user.name"]).await;
        let user = (!user_raw.is_empty()).then_some(user_raw);
        return Some(GitInfo {
            branch: None,
            ahead: 0,
            behind: 0,
            staged: 0,
            modified: 0,
            untracked: 0,
            conflicts: 0,
            is_empty: true,
            recent_commits: vec![],
            user,
        });
    }
    let branch = (branch_raw == "HEAD")
        .then(|| "HEAD (detached)".to_string())
        .or(Some(branch_raw));

    let ahead = git_out(cwd, &["rev-list", "--count", "@{upstream}..HEAD"])
        .await
        .parse::<u32>()
        .unwrap_or(0);
    let behind = git_out(cwd, &["rev-list", "--count", "HEAD..@{upstream}"])
        .await
        .parse::<u32>()
        .unwrap_or(0);

    let porcelain = git_out(cwd, &["status", "--porcelain"]).await;
    let mut staged = 0u32;
    let mut modified = 0u32;
    let mut untracked = 0u32;
    let mut conflicts = 0u32;
    for line in porcelain.lines() {
        let b = line.as_bytes();
        if b.len() < 2 {
            continue;
        }
        let x = b[0] as char;
        let y = b[1] as char;
        if x == '?' && y == '?' {
            untracked += 1;
        } else if x == 'U' || y == 'U' {
            conflicts += 1;
        } else {
            if x != ' ' && x != '?' {
                staged += 1;
            }
            if y != ' ' && y != '?' {
                modified += 1;
            }
        }
    }

    let log = git_out(
        cwd,
        &[
            "log",
            "-40",
            "--date=short",
            "--format=%h%x09%an%x09%ad%x09%s",
        ],
    )
    .await;
    let recent_commits: Vec<CommitInfo> = log.lines().filter_map(parse_commit_line).collect();
    let is_empty = recent_commits.is_empty();
    let user_raw = git_out(cwd, &["config", "user.name"]).await;
    let user = (!user_raw.is_empty()).then_some(user_raw);

    Some(GitInfo {
        branch,
        ahead,
        behind,
        staged,
        modified,
        untracked,
        conflicts,
        is_empty,
        recent_commits,
        user,
    })
}

async fn git_out(cwd: &Path, args: &[&str]) -> String {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(cwd).args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    match cmd.output().await {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// Parse a `git log --format=%h\t%an\t%ad\t%s` line into a [`CommitInfo`].
fn parse_commit_line(line: &str) -> Option<CommitInfo> {
    let mut it = line.splitn(4, '\t');
    Some(CommitInfo {
        sha: it.next()?.to_string(),
        author: it.next()?.to_string(),
        date: it.next()?.to_string(),
        subject: it.next().unwrap_or("").to_string(),
    })
}

/// `git show` a commit's stat + patch, capped (large diffs truncate). `sha` is
/// validated to hex 4-40 chars to keep it from being an arbitrary git arg.
pub async fn git_show(cwd: &Path, sha: &str) -> Option<String> {
    if sha.is_empty() || sha.len() > 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let out = git_out(cwd, &["show", "--stat", "--patch", "--no-color", sha]).await;
    if out.is_empty() {
        return None;
    }
    Some(truncate_chars(&out, 6000))
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn command_and_hook_sources_hide_arguments_queries_and_headers() {
        // **Validates: Requirements 9.8, 11.1, 11.2**
        let command = command_source("/usr/local/bin/mcp-server", 3);
        assert_eq!(command, "mcp-server (3 argument(s), values hidden)");
        assert_eq!(
            command_source("https://user:secret@example.test/run", 1),
            "configured command (1 argument(s), values hidden)"
        );

        let hook = nonoclaw_engine::hooks::HookDef {
            matcher: "*".into(),
            command: String::new(),
            args: vec![],
            prompt: None,
            http: Some(nonoclaw_engine::hooks::HttpHookConfig {
                url: "https://hooks.example.test/private?token=top-secret".into(),
                headers: [("Authorization".into(), "Bearer secret".into())]
                    .into_iter()
                    .collect(),
            }),
            timeout_secs: None,
            failure_policy: nonoclaw_engine::hooks::HookFailurePolicy::Continue,
        };
        let source = hook_source(&hook);
        assert_eq!(
            source,
            "http · https://hooks.example.test (path, query, and headers hidden)"
        );
        assert!(!source.contains("secret"));
        assert!(!source.contains("private"));
    }

    #[test]
    fn cli_reference_is_generated_from_the_clap_definition() {
        // **Validates: Requirements 12.2**
        let reference = cli_reference();
        assert!(reference.iter().any(|item| item.name == "-p, --print"));
        assert!(reference
            .iter()
            .any(|item| item.name == "--serve-http <ADDR>"));
        assert!(reference.iter().any(|item| item.name == "--mcp-serve"));
        assert!(!reference.iter().any(|item| item.name.contains("--bridge")));
        assert!(reference.iter().all(|item| !item.description.is_empty()));
    }

    #[test]
    fn public_metadata_placeholders_do_not_contain_raw_prompts() {
        let tool_prompt = "[tool prompt hidden]";
        let skill_body = "[skill content kept server-side]";
        assert!(!tool_prompt.contains("Bearer"));
        assert!(!skill_body.contains("sk-proj"));
    }
}
