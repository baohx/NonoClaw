//! Project-context gatherer for the web UI "Insight" rail + Git pane.
//!
//! Collects, in one bounded pass, everything worth showing about the current
//! project: the registered tools (built-in + MCP), MCP servers, skills,
//! plugins, the layered NONOCLAW.md / settings files, and a structured git
//! snapshot. Sent to the browser as `ServerMsg::ProjectInfo`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nonoclaw_tools::{McpServerConfig, ToolRegistry};
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
    pub hooks: Vec<HookEntry>,
    pub docs: Vec<PathLayer>,
    pub settings: Vec<PathLayer>,
    pub git: Option<GitInfo>,
    /// Configured model context window (tokens), if set.
    pub context_window: Option<usize>,
    /// Effective auto-compact threshold (tokens).
    pub compact_threshold: usize,
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
}

#[derive(Debug, Serialize, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub dir: String,   // abs install dir
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
    pub path: String,  // absolute, for click-to-open
    pub exists: bool,
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
    if let Some(v) = std::env::var_os("NONOCLAW_HOME") {
        return Some(PathBuf::from(v));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".nonoclaw"))
}

/// Gather the full project context. All local FS probes + a handful of git
/// subprocess calls; bounded (commits ≤10, prompt_preview ≤400 chars).
pub async fn gather(
    cwd: &Path,
    model: &str,
    registry: &ToolRegistry,
    mcp_configs: &[(String, McpServerConfig)],
    mcp_sources: &HashMap<String, String>,
    context_window: Option<usize>,
    compact_threshold: usize,
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
                prompt_preview: truncate_chars(t.prompt(), 400),
                input_schema: t.input_schema(),
            }
        })
        .collect();

    // MCP servers: derive connected/tool_count from what actually registered.
    let mcp_servers = mcp_configs
        .iter()
        .map(|(name, cfg)| {
            let tool_count = tools
                .iter()
                .filter(|t| t.mcp_server.as_deref() == Some(name.as_str()))
                .count();
            McpServerInfo {
                name: name.clone(),
                command: format!("{} {}", cfg.command, cfg.args.join(" ")),
                config_source: mcp_sources.get(name).cloned(),
                connected: tool_count > 0,
                tool_count,
            }
        })
        .collect();

    let skills = crate::skills::discover(cwd)
        .into_iter()
        .map(|s| SkillInfo {
            name: s.name,
            description: s.description,
            source: s.source,
        })
        .collect();

    let plugins = list_plugins();
    let hooks = nonoclaw_engine::hooks::load_hooks(cwd)
        .into_iter()
        .map(|(t, d)| HookEntry {
            hook_type: t.to_string(),
            matcher: d.matcher.clone(),
            command: format!("{} {}", d.command, d.args.join(" ")),
        })
        .collect();

    let docs = collect_doc_layers(cwd);
    let settings = collect_settings_layers(cwd);
    let git = git_info(cwd).await;

    ProjectInfo {
        cwd: cwd.to_string_lossy().to_string(),
        model: model.to_string(),
        tools,
        mcp_servers,
        skills,
        plugins,
        hooks,
        docs,
        settings,
        git,
        context_window,
        compact_threshold,
    }
}

/// Scan `~/.nonoclaw/plugins/<name>`; count each plugin's contributed skills.
fn list_plugins() -> Vec<PluginInfo> {
    let Some(home) = nonoclaw_home() else {
        return Vec::new();
    };
    let plugins_dir = home.join("plugins");
    let Ok(entries) = std::fs::read_dir(&plugins_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry
            .file_name()
            .to_string_lossy()
            .to_string();
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

    push(&mut out, "Project · NONOCLAW.md", cwd.join(".nonoclaw/NONOCLAW.md"));
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
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("rule.md");
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
    if branch_raw.is_empty() {
        return None; // not a git repo
    }
    let branch = (branch_raw == "HEAD").then(|| "HEAD (detached)".to_string()).or(Some(branch_raw));

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
        &["log", "-40", "--date=short", "--format=%h%x09%an%x09%ad%x09%s"],
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
    if sha.is_empty()
        || sha.len() > 40
        || !sha.chars().all(|c| c.is_ascii_hexdigit())
    {
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
