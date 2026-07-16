//! Skill discovery, parsing, and dynamic activation. Mirrors the CC TypeScript
//! `src/skills/loadSkillsDir.ts` architecture:
//!
//! - **Static skills** — discovered at startup, always available via `/name`.
//! - **Conditional skills** — have `paths` frontmatter; deferred until a matching
//!   file is operated on (Read/Write/Edit).
//! - **Dynamic skills** — discovered mid-session by walking up from file paths, or
//!   activated when a conditional skill's paths match.
//!
//! The [`SkillsManager`] is the thread-safe state container shared between the
//! engine loop and the HTTP server.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

mod bundled;

// ── Usage tracking ──────────────────────────────────────────────────────────

/// Records how often and when each skill is used. Persisted to disk for
/// cross-session ranking. Mirrors CC's `skillUsageTracking.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillUsageEntry {
    count: u32,
    last_used_secs: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillUsageData {
    entries: HashMap<String, SkillUsageEntry>,
}

/// Tracks skill invocations with 7-day half-life decay for ranking.
pub struct SkillUsageTracker {
    entries: HashMap<String, SkillUsageEntry>,
    path: PathBuf,
}

impl SkillUsageTracker {
    /// Load usage data from `~/.nonoclaw/skill-usage.json`, or start empty.
    pub fn load() -> Self {
        let path = usage_path();
        let entries = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<SkillUsageData>(&s).ok())
                .map(|d| d.entries)
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        SkillUsageTracker { entries, path }
    }

    /// Record a skill invocation.
    pub fn record(&mut self, name: &str) {
        let now = chrono::Utc::now().timestamp();
        let entry = self
            .entries
            .entry(name.to_string())
            .or_insert(SkillUsageEntry {
                count: 0,
                last_used_secs: now,
            });
        entry.count += 1;
        entry.last_used_secs = now;
    }

    /// Decay-weighted score: `count * 2^(-days_since_last_use / 7)`.
    pub fn score(&self, name: &str) -> f64 {
        if let Some(e) = self.entries.get(name) {
            let now = chrono::Utc::now().timestamp();
            let days = (now - e.last_used_secs) as f64 / 86400.0;
            e.count as f64 * 2.0_f64.powf(-days / 7.0)
        } else {
            0.0
        }
    }

    /// Return skill names sorted by descending score.
    pub fn sorted_names(&self) -> Vec<String> {
        let mut names: Vec<(String, f64)> = self
            .entries
            .keys()
            .map(|k| (k.clone(), self.score(k)))
            .collect();
        names.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        names.into_iter().map(|(n, _)| n).collect()
    }

    /// Persist to disk.
    pub fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let data = SkillUsageData {
            entries: self.entries.clone(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&data) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

fn usage_path() -> PathBuf {
    nonoclaw_core::nonoclaw_data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("skill-usage.json")
}

// ── Skill ────────────────────────────────────────────────────────────────────

/// One discovered/activated skill. Mirrors CC's `Command` (prompt-type) + all
/// frontmatter fields from `parseSkillFrontmatterFields`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Full markdown body: SKILL.md content + appended reference .md files.
    pub body: String,
    /// Directory containing SKILL.md (the "skill root").
    pub source: String,

    // ── Conditional activation ────────────────────────────────────────────
    /// Glob patterns from `paths` frontmatter. When non-empty the skill is
    /// **conditional** — it is stored separately and only activated when the
    /// model reads/writes/edits a matching file.
    #[serde(default)]
    pub paths: Vec<String>,

    /// Regex patterns from `triggers` frontmatter. When user input matches any
    /// pattern the skill is auto-activated.
    #[serde(default)]
    pub triggers: Vec<String>,

    // ── Metadata ──────────────────────────────────────────────────────────
    /// NL instruction for when the model should invoke this skill.
    #[serde(default)]
    pub when_to_use: Option<String>,

    /// Tools this skill is allowed to use (empty = all).
    #[serde(default)]
    pub allowed_tools: Vec<String>,

    /// CLI argument hint shown in autocomplete.
    #[serde(default)]
    pub argument_hint: Option<String>,

    /// Positional argument names for `$arg` substitution.
    #[serde(default)]
    pub argument_names: Vec<String>,

    /// Skill version string.
    #[serde(default)]
    pub version: Option<String>,

    /// Override the default model when this skill is active.
    #[serde(default)]
    pub model: Option<String>,

    /// If true, the model cannot invoke this skill on its own — slash-command only.
    #[serde(default)]
    pub disable_model_invocation: bool,

    /// Whether the user can type `/name` to invoke (defaults true).
    #[serde(default = "default_true")]
    pub user_invocable: bool,

    /// Execution context: `"fork"` spawns a sub-agent; `None` means inline.
    #[serde(default)]
    pub context: Option<String>,

    /// Agent type when context is `"fork"`.
    #[serde(default)]
    pub agent: Option<String>,

    /// Thinking effort level (e.g. "low", "medium", "high", "xhigh", "max").
    #[serde(default)]
    pub effort: Option<String>,

    /// Shell override: `"bash"` or `"powershell"`.
    #[serde(default)]
    pub shell: Option<String>,
}

impl Default for Skill {
    fn default() -> Self {
        Skill {
            name: String::new(),
            description: String::new(),
            body: String::new(),
            source: String::new(),
            paths: Vec::new(),
            triggers: Vec::new(),
            when_to_use: None,
            allowed_tools: Vec::new(),
            argument_hint: None,
            argument_names: Vec::new(),
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            context: None,
            agent: None,
            effort: None,
            shell: None,
        }
    }
}

const fn default_true() -> bool {
    true
}

impl Skill {
    /// Whether this skill should be treated as conditional (deferred until
    /// matching files are touched).
    pub fn is_conditional(&self) -> bool {
        !self.paths.is_empty()
    }
}

// ── SkillsManager ────────────────────────────────────────────────────────────

/// Thread-safe container for skill state. Shared between the engine loop and
/// the HTTP/WS server via `Arc<RwLock<SkillsManager>>`.
///
/// Three collections:
/// 1. `static_skills` — discovered at startup, no `paths` → always active.
/// 2. `conditional_skills` — have `paths` → deferred until matching file ops.
/// 3. `dynamic_skills` — activated conditionally or discovered mid-session.
pub struct SkillsManager {
    static_skills: Vec<Skill>,
    conditional_skills: HashMap<String, Skill>,
    dynamic_skills: HashMap<String, Skill>,
    dynamic_skill_dirs: HashSet<PathBuf>,
    activated_conditional_names: HashSet<String>,
    /// Monotonic counter incremented on any state change; consumers poll this
    /// to know when to rebuild the system prompt.
    version: AtomicU64,
    /// Skill usage tracker for ranking (loaded from disk).
    usage: SkillUsageTracker,
}

impl SkillsManager {
    // ── Construction ──────────────────────────────────────────────────────

    /// Discover all skills under `cwd` and separate into static vs conditional.
    pub fn new(cwd: &Path) -> Self {
        let mut all = discover(cwd);
        // Register bundled (built-in) skills first — disk skills can override them.
        bundled::register_bundled(&mut all);
        let mut static_skills = Vec::new();
        let mut conditional_skills = HashMap::new();

        for mut skill in all {
            // Normalize paths: strip `**` (match-all) and trailing `/`.
            let paths: Vec<String> = skill
                .paths
                .iter()
                .map(|p| p.trim_end_matches('/').to_string())
                .filter(|p| p != "**" && !p.is_empty())
                .collect();

            if paths.is_empty() {
                skill.paths = Vec::new();
                static_skills.push(skill);
            } else {
                skill.paths = paths;
                conditional_skills.insert(skill.name.clone(), skill);
            }
        }

        SkillsManager {
            static_skills,
            conditional_skills,
            dynamic_skills: HashMap::new(),
            dynamic_skill_dirs: HashSet::new(),
            activated_conditional_names: HashSet::new(),
            version: AtomicU64::new(0),
            usage: SkillUsageTracker::load(),
        }
    }

    // ── Queries ──────────────────────────────────────────────────────────

    /// All **currently active** skills (static + dynamic), deduped by name
    /// (dynamic takes precedence over static).
    pub fn all_active(&self) -> Vec<Skill> {
        let mut seen: HashMap<String, Skill> = HashMap::new();
        for s in &self.static_skills {
            seen.insert(s.name.clone(), s.clone());
        }
        for s in self.dynamic_skills.values() {
            seen.insert(s.name.clone(), s.clone());
        }
        let mut out: Vec<Skill> = seen.into_values().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Number of pending conditional skills (not yet activated).
    pub fn conditional_count(&self) -> usize {
        self.conditional_skills.len()
    }

    /// Current monotonic version — changes whenever skill state changes.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Save usage data to disk (called on graceful shutdown).
    pub fn save_usage(&self) {
        self.usage.save();
    }

    /// Re-scan all skill directories and add newly discovered skills to
    /// `dynamic_skills`.  Called when the user explicitly refreshes the
    /// Insight panel (file watcher may miss new subdirectories).
    pub fn rescan(&mut self, cwd: &Path) {
        let discovered = discover(cwd);
        for skill in discovered {
            if !self.static_skills.iter().any(|s| s.name == skill.name)
                && !self.dynamic_skills.contains_key(&skill.name)
            {
                tracing::info!(name = skill.name, "rescan discovered new skill");
                self.dynamic_skills.insert(skill.name.clone(), skill);
                self.bump();
            }
        }
    }

    /// All active skills sorted by usage frequency (highest score first).
    pub fn ranked_active(&self) -> Vec<Skill> {
        let mut active = self.all_active();
        let ranked = self.usage.sorted_names();
        // Sort: ranked names first (by score), then unranked alphabetically.
        let rank_map: HashMap<&str, usize> = ranked
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        active.sort_by(|a, b| {
            match (rank_map.get(a.name.as_str()), rank_map.get(b.name.as_str())) {
                (Some(ai), Some(bi)) => ai.cmp(bi),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.name.cmp(&b.name),
            }
        });
        active
    }

    // ── Prompt rendering ─────────────────────────────────────────────────

    /// Format all active skills as a system-prompt block. Includes
    /// `when_to_use` guidance so the model knows when to invoke each skill.
    pub fn render_prompt(&self) -> String {
        let active = self.all_active();
        if active.is_empty() {
            return String::new();
        }

        let mut out = String::from("# Available Skills\n\n");
        for skill in &active {
            out.push_str(&format!("## {}\n", skill.name));
            if !skill.description.is_empty() {
                out.push_str(&format!("**Description**: {}\n", skill.description));
            }
            if let Some(ref wtu) = skill.when_to_use {
                out.push_str(&format!(
                    "**When to use**: {}\n",
                    wtu.trim()
                ));
            }
            if !skill.argument_names.is_empty() {
                out.push_str(&format!(
                    "**Arguments**: {}\n",
                    skill.argument_names.join(", ")
                ));
            }
            if let Some(ref hint) = skill.argument_hint {
                out.push_str(&format!("**Usage**: /{} {}\n", skill.name, hint));
            }
            out.push('\n');
            out.push_str(&skill.body);
            if !skill.body.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("\n---\n\n");
        }
        // The engine will append this block to the system prompt; strip the
        // trailing separator line to avoid an unnecessary blank section.
        while out.ends_with("\n---\n\n") {
            let end = out.len().saturating_sub("\n---\n\n".len());
            out.truncate(end);
        }
        out
    }

    /// Find a skill by name across all three collections.
    pub fn get_skill(&self, name: &str) -> Option<Skill> {
        if let Some(s) = self.dynamic_skills.get(name) {
            return Some(s.clone());
        }
        if let Some(s) = self.conditional_skills.get(name) {
            return Some(s.clone());
        }
        self.static_skills.iter().find(|s| s.name == name).cloned()
    }

    /// Render a single skill's body with argument substitution applied.
    /// Returns `None` if the skill is not found.
    pub fn render_skill_with_args(
        &self,
        name: &str,
        args: &str,
        session_id: &str,
    ) -> Option<String> {
        let skill = self.get_skill(name)?;
        let skill_dir = if skill.source.is_empty() {
            None
        } else {
            Some(skill.source.as_str())
        };
        Some(substitute_arguments(
            &skill.body,
            args,
            &skill.argument_names,
            skill_dir,
            Some(session_id),
        ))
    }

    // ── Mutations ────────────────────────────────────────────────────────

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// Force-activate a conditional skill by name (user typed `/skill-name`).
    /// Returns true if the skill was found and activated.
    pub fn activate_slash_command(&mut self, name: &str) -> bool {
        self.usage.record(name);
        if let Some(skill) = self.conditional_skills.remove(name) {
            tracing::info!(name, "slash-command activated conditional skill");
            self.activated_conditional_names.insert(name.to_string());
            self.dynamic_skills
                .insert(skill.name.clone(), skill);
            self.bump();
            true
        } else {
            // Already active (static or dynamic) — still count as usage.
            false
        }
    }

    /// Check each conditional skill's `paths` against the given file paths.
    /// On match: move from conditional to dynamic, return activated names.
    pub fn activate_conditional_for_paths(
        &mut self,
        file_paths: &[PathBuf],
        cwd: &Path,
    ) -> Vec<String> {
        if self.conditional_skills.is_empty() || file_paths.is_empty() {
            return Vec::new();
        }

        let mut activated = Vec::new();
        let mut to_activate: Vec<String> = Vec::new();

        for (name, skill) in &self.conditional_skills {
            if skill.paths.is_empty() {
                continue;
            }
            for fp in file_paths {
                let rel = relative_to(fp, cwd);
                if rel.is_empty() || rel.starts_with("..") {
                    continue;
                }
                if skill.paths.iter().any(|p| matches_pattern(p, &rel)) {
                    to_activate.push(name.clone());
                    break;
                }
            }
        }

        for name in &to_activate {
            if let Some(skill) = self.conditional_skills.remove(name) {
                tracing::info!(
                    name,
                    "conditionally activated skill (matched file path)"
                );
                self.activated_conditional_names.insert(name.clone());
                self.dynamic_skills.insert(skill.name.clone(), skill);
                self.usage.record(name);
                activated.push(name.clone());
            }
        }

        if !activated.is_empty() {
            self.bump();
        }
        activated
    }

    /// Walk up from each file path (stopping below `cwd`) to discover
    /// `.nonoclaw/skills/` directories. Load any new ones into dynamic skills.
    /// Returns newly discovered directory paths.
    pub fn discover_for_file_paths(
        &mut self,
        file_paths: &[PathBuf],
        cwd: &Path,
    ) -> Vec<PathBuf> {
        let resolved_cwd = canonical(cwd);
        let mut new_dirs: Vec<PathBuf> = Vec::new();

        for fp in file_paths {
            let mut current = if fp.is_dir() {
                fp.clone()
            } else {
                fp.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| fp.clone())
            };

            // Walk up to cwd's parent (NOT including cwd itself — cwd-level
            // skills are already loaded at startup).
            while let Ok(rest) = current.strip_prefix(&resolved_cwd) {
                // Stop at cwd itself; only discover nested dirs.
                if rest.as_os_str().is_empty() {
                    break;
                }
                let skill_dir = current.join(".nonoclaw").join("skills");
                if skill_dir.is_dir() && self.dynamic_skill_dirs.insert(skill_dir.clone()) {
                    new_dirs.push(skill_dir.clone());
                }
                if let Some(parent) = current.parent() {
                    current = parent.to_path_buf();
                } else {
                    break;
                }
            }
        }

        // Sort deepest-first so skills closer to the file take precedence.
        new_dirs.sort_by(|a, b| {
            b.as_os_str().len().cmp(&a.as_os_str().len())
        });

        if !new_dirs.is_empty() {
            for dir in &new_dirs {
                self.load_from_dir(dir);
            }
            self.bump();
        }

        new_dirs
    }

    /// Scan a single skills directory, parse all `SKILL.md` files, and insert
    /// them into `dynamic_skills` (later loads override earlier by name).
    pub fn load_from_dir(&mut self, dir: &Path) {
        let loaded = scan_skill_dir(dir);
        for skill in loaded {
            tracing::debug!(
                name = skill.name,
                dir = %dir.display(),
                "dynamically loaded skill"
            );
            self.dynamic_skills.insert(skill.name.clone(), skill);
        }
    }

    /// Check all skills (static + conditional + dynamic) for `triggers` regex
    /// patterns matching `user_input`. Activate any matching conditional skills.
    /// Returns names of matched skills.
    pub fn match_triggers(&mut self, user_input: &str) -> Vec<String> {
        if user_input.trim().is_empty() {
            return Vec::new();
        }

        let mut matched = Vec::new();

        // Check static skills
        for s in &self.static_skills {
            if triggers_match(&s.triggers, user_input) {
                matched.push(s.name.clone());
            }
        }
        // Check dynamic skills
        for s in self.dynamic_skills.values() {
            if triggers_match(&s.triggers, user_input)
                && !matched.contains(&s.name)
            {
                matched.push(s.name.clone());
            }
        }
        // Check conditional skills — auto-activate if triggered
        let mut to_activate: Vec<String> = Vec::new();
        for (name, s) in &self.conditional_skills {
            if triggers_match(&s.triggers, user_input) {
                if !matched.contains(name) {
                    matched.push(name.clone());
                }
                to_activate.push(name.clone());
            }
        }
        for name in &to_activate {
            if let Some(skill) = self.conditional_skills.remove(name) {
                tracing::info!(name, "trigger-activated conditional skill");
                self.activated_conditional_names.insert(name.clone());
                self.dynamic_skills.insert(skill.name.clone(), skill);
                self.usage.record(name);
            }
        }

        if !to_activate.is_empty() {
            self.bump();
        }
        matched
    }
}

// ── Discovery ────────────────────────────────────────────────────────────────

/// Discover skills for `cwd`: project `.nonoclaw/skills`, user
/// `~/.nonoclaw/skills`, and plugin-contributed skills. Later sources override
/// earlier ones by name.
fn discover(cwd: &Path) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();

    let mut bases: Vec<PathBuf> = vec![cwd.join(".nonoclaw").join("skills")];
    if let Some(home) = nonoclaw_core::nonoclaw_data_dir() {
        bases.push(home.join("skills"));
    }
    for base in &bases {
        for skill in scan_skill_dir(base) {
            skills.push(skill);
        }
    }

    // Plugin-contributed skills
    let plugins_dir = cwd.join(".nonoclaw").join("plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            let plugin_skills = entry.path().join("skills");
            for skill in scan_skill_dir(&plugin_skills) {
                skills.push(skill);
            }
        }
    }

    dedup_by_name(skills)
}

fn scan_skill_dir(base: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let skill_md = path.join("SKILL.md");
        if let Some(skill) = parse_skill(&skill_md) {
            out.push(skill);
        }
    }
    out
}

// ── Parsing ─────────────────────────────────────────────────────────────────

/// Parse a SKILL.md file: YAML frontmatter then markdown body.
pub fn parse_skill(path: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let fallback_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());
    let source = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let mut skill = parse_skill_str(&text, fallback_name.as_deref(), &source)?;

    // Load reference .md files from the skill directory (file-system only).
    if let Some(skill_dir) = path.parent() {
        let mut refs: Vec<PathBuf> = Vec::new();
        walk_ref_dir(skill_dir, &mut refs);
        refs.sort();
        for ref_path in &refs {
            if ref_path == path {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(ref_path) {
                let rel = ref_path
                    .strip_prefix(skill_dir)
                    .unwrap_or(ref_path)
                    .display();
                skill.body.push_str(&format!("\n\n## {rel}\n{content}"));
            }
        }
    }

    Some(skill)
}

/// Parse a SKILL.md body from an in-memory string (used for bundled skills
/// embedded via `include_str!()`). `fallback_name` is the directory name
/// (used when frontmatter omits `name`). `source` is a human-readable label
/// like `"bundled:code-review"`.
pub fn parse_skill_str(
    text: &str,
    fallback_name: Option<&str>,
    source: &str,
) -> Option<Skill> {
    let (front, body) = parse_frontmatter(text);

    let name = front
        .get("name")
        .cloned()
        .or_else(|| fallback_name.map(|s| s.to_string()))?;

    let description = front.get("description").cloned().unwrap_or_default();
    let full_body = body.trim().to_string();

    let paths = parse_string_list(front.get("paths"));
    let triggers = parse_string_list(front.get("triggers"));
    let when_to_use = front.get("when_to_use").cloned();
    let allowed_tools = parse_string_list(front.get("allowed-tools"));
    let argument_hint = front
        .get("argument-hint")
        .or_else(|| front.get("argument_hint"))
        .cloned();
    let argument_names = parse_string_list(
        front
            .get("arguments")
            .or_else(|| front.get("argument_names")),
    );
    let version = front.get("version").cloned();
    let model = front.get("model").cloned();
    let disable_model_invocation = parse_bool(front.get("disable-model-invocation"));
    let user_invocable = front
        .get("user-invocable")
        .map(|v| parse_bool(Some(v)))
        .unwrap_or(true);
    let context = front.get("context").cloned();
    let agent = front.get("agent").cloned();
    let effort = front.get("effort").cloned();
    let shell = front.get("shell").cloned();

    Some(Skill {
        name,
        description,
        body: full_body,
        source: source.to_string(),
        paths,
        triggers,
        when_to_use,
        allowed_tools,
        argument_hint,
        argument_names,
        version,
        model,
        disable_model_invocation,
        user_invocable,
        context,
        agent,
        effort,
        shell,
    })
}

/// Parse YAML frontmatter `---\n...\n---\n<body>`. Uses `serde_yaml` for
/// robust parsing; falls back to line-by-line `key: value` if YAML fails.
fn parse_frontmatter(text: &str) -> (HashMap<String, String>, String) {
    let trimmed = text.trim_start_matches('\u{feff}');
    let rest = match trimmed.strip_prefix("---") {
        Some(r) => r,
        None => return (HashMap::new(), text.to_string()),
    };

    // Find closing `---`.
    let end_marker = match rest.find("\n---") {
        Some(pos) => pos,
        None => return (HashMap::new(), text.to_string()),
    };

    let yaml_str = &rest[..end_marker];
    let body = rest[end_marker + 4..]
        .trim_start_matches('\n')
        .trim_end()
        .to_string();

    // Try serde_yaml first.
    let map = if let Ok(value) =
        serde_yaml::from_str::<serde_yaml::Value>(yaml_str)
    {
        yaml_value_to_map(&value)
    } else {
        // Fallback: line-by-line key: value.
        legacy_parse_yaml(yaml_str)
    };

    (map, body.to_string())
}

/// Flatten a YAML value into `HashMap<String, String>` for simple frontmatter.
fn yaml_value_to_map(value: &serde_yaml::Value) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(mapping) = value.as_mapping() {
        for (k, v) in mapping {
            let key = k.as_str().unwrap_or("").to_string();
            let val = match v {
                serde_yaml::Value::String(s) => s.clone(),
                serde_yaml::Value::Bool(b) => b.to_string(),
                serde_yaml::Value::Number(n) => n.to_string(),
                serde_yaml::Value::Sequence(seq) => {
                    let items: Vec<String> = seq
                        .iter()
                        .map(|item| match item {
                            serde_yaml::Value::String(s) => s.clone(),
                            other => format!("{other:?}"),
                        })
                        .collect();
                    items.join(", ")
                }
                _ => format!("{v:?}"),
            };
            map.insert(key, val);
        }
    }
    map
}

/// Legacy line-by-line `key: value` parser (fallback when YAML fails).
fn legacy_parse_yaml(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !key.is_empty() {
                map.insert(key, val);
            }
        }
    }
    map
}

/// Parse a frontmatter value as a list: comma-separated string or YAML-like
/// `[a, b, c]`.
fn parse_string_list(raw: Option<&String>) -> Vec<String> {
    let raw = match raw {
        Some(s) => s,
        None => return Vec::new(),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    // YAML array syntax: [a, b, c]
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        return inner
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    // Comma-separated
    trimmed
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_bool(raw: Option<&String>) -> bool {
    match raw.map(|s| s.to_lowercase()).as_deref() {
        Some("true") | Some("1") | Some("yes") => true,
        _ => false,
    }
}

// ── Glob matching ────────────────────────────────────────────────────────────

/// Test whether `relative_path` matches a skill `pattern`.
/// Uses the `glob` crate for `**` patterns and single-level fallback for `*`.
///
/// CC's `parseSkillPaths` strips trailing `/**`, so a pattern `src/` matches
/// `src/` itself AND everything under it. We replicate that here.
fn matches_pattern(pattern: &str, relative_path: &str) -> bool {
    let pattern = pattern.trim_end_matches('/');

    // If pattern has no glob metacharacters, match as prefix.
    if !pattern.contains('*') && !pattern.contains('?') && !pattern.contains('[') {
        return relative_path == pattern
            || relative_path.starts_with(&format!("{pattern}/"));
    }

    // For `**` patterns, use the glob crate (handles cross-directory matching).
    if pattern.contains("**") {
        if let Ok(pat) = glob::Pattern::new(pattern) {
            return pat.matches(relative_path);
        }
        // Fallback: prefix match on the part before `/**`.
        let prefix = pattern.trim_end_matches("/**");
        return relative_path == prefix
            || relative_path.starts_with(&format!("{prefix}/"));
    }

    // For single-level `*` / `?` / `[abc]` patterns, use a custom matcher that
    // respects `/` boundaries. The glob crate v0.3 `*` semantics differ across
    // platforms, so we use our own `simple_glob` for single-level wildcards.
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        // Try char-by-char glob with `/` as barrier.
        return simple_glob_match(pattern, relative_path);
    }

    relative_path == pattern
}

/// Character-by-character glob match where `*` stops at `/` and `?` matches
/// exactly one non-`/` character.
fn simple_glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = path.chars().collect();
    let (pi, ti) = (0usize, 0usize);
    glob_match_recurse(&pat, &txt, pi, ti)
}

fn glob_match_recurse(pat: &[char], txt: &[char], pi: usize, ti: usize) -> bool {
    if pi >= pat.len() {
        return ti >= txt.len();
    }
    match pat[pi] {
        '*' => {
            // Match zero or more non-`/` characters.
            // Try matching zero chars, then grow the span.
            for end in ti..=txt.len() {
                if end > ti && txt[end - 1] == '/' {
                    // Don't let `*` consume past `/`.
                    break;
                }
                if glob_match_recurse(pat, txt, pi + 1, end) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if ti < txt.len() && txt[ti] != '/' {
                glob_match_recurse(pat, txt, pi + 1, ti + 1)
            } else {
                false
            }
        }
        '[' => {
            // Simple character class: [abc] or [a-z].
            let end = pat[pi..].iter().position(|&c| c == ']').map(|i| pi + i);
            if let Some(close) = end {
                if ti < txt.len() {
                    let ch = txt[ti];
                    let inner = &pat[pi + 1..close];
                    let matched = if inner.len() >= 3 && inner[1] == '-' {
                        let lo = inner[0];
                        let hi = inner[2];
                        ch >= lo && ch <= hi
                    } else {
                        inner.contains(&ch)
                    };
                    if matched {
                        return glob_match_recurse(pat, txt, close + 1, ti + 1);
                    }
                }
                false
            } else {
                // Malformed — treat `[` as literal.
                ti < txt.len() && txt[ti] == '[' && glob_match_recurse(pat, txt, pi + 1, ti + 1)
            }
        }
        c => {
            if ti < txt.len() && txt[ti] == c {
                glob_match_recurse(pat, txt, pi + 1, ti + 1)
            } else {
                false
            }
        }
    }
}

// ── Triggers matching ───────────────────────────────────────────────────────

fn triggers_match(patterns: &[String], input: &str) -> bool {
    if patterns.is_empty() || input.trim().is_empty() {
        return false;
    }
    patterns.iter().any(|p| {
        // Try as regex first, fall back to case-insensitive substring.
        if let Ok(re) = regex::Regex::new(p) {
            re.is_match(input)
        } else {
            input.to_lowercase().contains(&p.to_lowercase())
        }
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn relative_to(abs: &Path, base: &Path) -> String {
    abs.strip_prefix(base)
        .unwrap_or(abs)
        .to_string_lossy()
        .to_string()
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn walk_ref_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_ref_dir(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(p);
        }
    }
}

fn dedup_by_name(skills: Vec<Skill>) -> Vec<Skill> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Skill> = Vec::new();
    // Reverse iteration → first occurrence wins; we reverse back at the end.
    for s in skills.into_iter().rev() {
        if seen.insert(s.name.clone()) {
            out.push(s);
        }
    }
    out.reverse();
    out
}

/// Substitute argument placeholders in a skill body. Mirrors CC's
/// `argumentSubstitution.ts`.
///
/// Rules (in order):
/// 1. Named args: `$name` → positional value from `argument_names` index
/// 2. Indexed: `$0`, `$1`, `$ARGUMENTS[0]`, `$ARGUMENTS[1]` → positional
/// 3. Raw: `$ARGUMENTS` → full args string
/// 4. If no placeholders found → append `\n\nARGUMENTS: {args}`
/// 5. `${CLAUDE_SKILL_DIR}` → `skill_dir` (or `NONOCLAW_SKILL_DIR`)
/// 6. `${CLAUDE_SESSION_ID}` → `session_id` (or `NONOCLAW_SESSION_ID`)
pub fn substitute_arguments(
    body: &str,
    args: &str,
    argument_names: &[String],
    skill_dir: Option<&str>,
    session_id: Option<&str>,
) -> String {
    let parsed = parse_args(args);
    let mut result = body.to_string();
    let mut has_placeholder = false;

    // Named args: $name → positional
    for (i, arg_name) in argument_names.iter().enumerate() {
        let pattern = format!("${}", arg_name);
        if result.contains(&pattern) {
            has_placeholder = true;
            let val = parsed.get(i).map(|s| s.as_str()).unwrap_or("");
            result = result.replace(&pattern, val);
        }
    }

    // $ARGUMENTS[N] and $N
    let re_indexed = regex::Regex::new(r"\$ARGUMENTS\[(\d+)\]").unwrap();
    if re_indexed.is_match(&result) {
        has_placeholder = true;
        result = re_indexed
            .replace_all(&result, |caps: &regex::Captures| {
                let idx: usize = caps[1].parse().unwrap_or(0);
                parsed.get(idx).cloned().unwrap_or_default()
            })
            .to_string();
    }

    let re_dollar_n = regex::Regex::new(r"\$(\d+)(?!\w)").unwrap();
    if re_dollar_n.is_match(&result) {
        has_placeholder = true;
        result = re_dollar_n
            .replace_all(&result, |caps: &regex::Captures| {
                let idx: usize = caps[1].parse().unwrap_or(0);
                parsed.get(idx).cloned().unwrap_or_default()
            })
            .to_string();
    }

    // $ARGUMENTS → raw args
    if result.contains("$ARGUMENTS") {
        has_placeholder = true;
        result = result.replace("$ARGUMENTS", args);
    }

    // ${CLAUDE_SKILL_DIR} / ${NONOCLAW_SKILL_DIR}
    if let Some(dir) = skill_dir {
        if result.contains("${CLAUDE_SKILL_DIR}") {
            has_placeholder = true;
            result = result.replace("${CLAUDE_SKILL_DIR}", dir);
        }
        if result.contains("${NONOCLAW_SKILL_DIR}") {
            has_placeholder = true;
            result = result.replace("${NONOCLAW_SKILL_DIR}", dir);
        }
    }

    // ${CLAUDE_SESSION_ID} / ${NONOCLAW_SESSION_ID}
    if let Some(sid) = session_id {
        if result.contains("${CLAUDE_SESSION_ID}") {
            has_placeholder = true;
            result = result.replace("${CLAUDE_SESSION_ID}", sid);
        }
        if result.contains("${NONOCLAW_SESSION_ID}") {
            has_placeholder = true;
            result = result.replace("${NONOCLAW_SESSION_ID}", sid);
        }
    }

    // Fallback: if no placeholders were found, append raw args.
    if !has_placeholder && !args.is_empty() {
        result.push_str(&format!("\n\nARGUMENTS: {args}"));
    }

    result
}

/// Split args string into tokens using shell-like quoting rules.
fn parse_args(args: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = args.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
        } else if in_double {
            if ch == '"' {
                in_double = false;
            } else if ch == '\\' && i + 1 < chars.len() {
                i += 1;
                current.push(chars[i]);
            } else {
                current.push(ch);
            }
        } else if ch == '\'' {
            in_single = true;
        } else if ch == '"' {
            in_double = true;
        } else if ch == ' ' || ch == '\t' {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
        i += 1;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nonoclaw-skills-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    }

    // ── Frontmatter parsing ──────────────────────────────────────────────

    #[test]
    fn parses_yaml_frontmatter() {
        let text = "---\nname: my-skill\ndescription: does a thing\nversion: \"1.2\"\n---\nRun the thing now.\n";
        let (front, body) = parse_frontmatter(text);
        assert_eq!(front.get("name").unwrap(), "my-skill");
        assert_eq!(front.get("description").unwrap(), "does a thing");
        assert_eq!(front.get("version").unwrap(), "1.2");
        assert_eq!(body, "Run the thing now.");
    }

    #[test]
    fn parses_yaml_lists() {
        let text = "---\nname: my-skill\npaths:\n  - \"src/**/*.rs\"\n  - \"tests/*.rs\"\nallowed-tools: [Read, Write, Bash]\n---\nbody\n";
        let (front, _) = parse_frontmatter(text);
        assert_eq!(front.get("name").unwrap(), "my-skill");
        // YAML lists get joined by ", "
        assert!(front.get("paths").unwrap().contains("src/**/*.rs"));
        assert!(front.get("paths").unwrap().contains("tests/*.rs"));
    }

    #[test]
    fn parses_boolean_fields() {
        let text = "---\nname: s\ndisable-model-invocation: true\nuser-invocable: false\n---\nbody\n";
        let (front, _) = parse_frontmatter(text);
        assert_eq!(front.get("disable-model-invocation").unwrap(), "true");
        assert_eq!(front.get("user-invocable").unwrap(), "false");
    }

    #[test]
    fn falls_back_legacy_without_yaml_delimiters() {
        let text = "name: fallback\ndescription: test\n---\nbody here\n";
        let (front, body) = parse_frontmatter(text);
        // No leading --- so fallback: whole text is body.
        assert!(front.is_empty());
        assert!(body.contains("name: fallback"));
    }

    #[test]
    fn fallback_without_frontmatter() {
        let dir = tempdir();
        write_skill(&dir, "myskill", "just body, no frontmatter");
        let s = parse_skill(&dir.join("myskill").join("SKILL.md")).unwrap();
        assert_eq!(s.name, "myskill");
        assert_eq!(s.body, "just body, no frontmatter");
    }

    #[test]
    fn parses_full_skill() {
        let dir = tempdir();
        let skill_dir = dir.join("deploy");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: deploys the app\npaths:\n  - deploy/**\nwhen_to_use: when user wants to deploy\nallowed-tools: [Bash, Read]\ndisable-model-invocation: false\nuser-invocable: true\n---\n\nRun `./deploy.sh` to deploy.\n",
        )
        .unwrap();
        let s = parse_skill(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(s.name, "deploy");
        assert_eq!(s.description, "deploys the app");
        assert_eq!(s.paths, vec!["deploy/**"]);
        assert_eq!(s.when_to_use.as_deref(), Some("when user wants to deploy"));
        assert_eq!(s.allowed_tools, vec!["Bash", "Read"]);
        assert!(!s.disable_model_invocation);
        assert!(s.user_invocable);
        assert!(s.body.contains("Run `./deploy.sh`"));
    }

    #[test]
    fn parses_triggers() {
        let text = "---\nname: s\ntriggers:\n  - \"deploy|ship|release\"\n  - \"prod\"\n---\nbody\n";
        let (front, _) = parse_frontmatter(text);
        assert!(front.get("triggers").unwrap().contains("deploy|ship|release"));
        assert!(front.get("triggers").unwrap().contains("prod"));
    }

    // ── Glob matching ────────────────────────────────────────────────────

    #[test]
    fn glob_star_star_matches_recursive() {
        assert!(matches_pattern("src/**", "src/main.rs"));
        assert!(matches_pattern("src/**", "src/sub/mod.rs"));
        assert!(!matches_pattern("src/**", "tests/main.rs"));
    }

    #[test]
    fn glob_exact_match() {
        assert!(matches_pattern("README.md", "README.md"));
        assert!(!matches_pattern("README.md", "src/README.md"));
    }

    #[test]
    fn glob_star_single_level() {
        assert!(matches_pattern("src/*.rs", "src/main.rs"));
        assert!(!matches_pattern("src/*.rs", "src/sub/mod.rs"));
    }

    #[test]
    fn glob_prefix_match_no_wildcard() {
        // Pattern without glob chars matches as prefix.
        assert!(matches_pattern("src", "src/main.rs"));
        assert!(matches_pattern("src", "src"));
        assert!(!matches_pattern("src", "tests/main.rs"));
    }

    #[test]
    fn glob_empty_paths() {
        let text = "---\nname: s\npaths: [\"**\"]\n---\nbody\n";
        let (front, _) = parse_frontmatter(text);
        // "**" should be filtered out as match-all
        let paths = parse_string_list(front.get("paths"));
        // parse_string_list returns ["**"], filtering happens in SkillsManager::new
        assert_eq!(paths, vec!["**"]);
    }

    // ── SkillsManager ────────────────────────────────────────────────────

    #[test]
    fn separates_static_vs_conditional() {
        let dir = tempdir();
        let skills_dir = dir.join(".nonoclaw").join("skills");
        std::fs::create_dir_all(skills_dir.join("always")).unwrap();
        std::fs::write(
            skills_dir.join("always").join("SKILL.md"),
            "---\nname: always\ndescription: a\n---\nbody a",
        )
        .unwrap();
        std::fs::create_dir_all(skills_dir.join("conditional")).unwrap();
        std::fs::write(
            skills_dir.join("conditional").join("SKILL.md"),
            "---\nname: conditional\ndescription: c\npaths: [\"src/**/*.rs\"]\n---\nbody c",
        )
        .unwrap();

        let mgr = SkillsManager::new(&dir);
        // Static skills should include "always" (plus bundled + ~/.nonoclaw).
        assert!(mgr.static_skills.iter().any(|s| s.name == "always"));
        // Conditional skills include our "conditional" (plus any bundled with paths).
        assert!(mgr.conditional_skills.contains_key("conditional"));
    }

    #[test]
    fn activate_conditional_for_paths() {
        let dir = tempdir();
        let skills_dir = dir.join(".nonoclaw").join("skills");
        std::fs::create_dir_all(skills_dir.join("rust-helper")).unwrap();
        std::fs::write(
            skills_dir.join("rust-helper").join("SKILL.md"),
            "---\nname: rust-helper\ndescription: helps with Rust\npaths: [\"src/**/*.rs\"]\n---\nRust help body",
        )
        .unwrap();

        let mut mgr = SkillsManager::new(&dir);
        let cond_before = mgr.conditional_count();
        assert!(mgr.conditional_skills.contains_key("rust-helper"));

        // No match — wrong file type.
        let activated = mgr.activate_conditional_for_paths(
            &[PathBuf::from("src/readme.md")],
            &dir,
        );
        assert!(activated.is_empty());
        assert_eq!(mgr.conditional_count(), cond_before);

        // Match — .rs file activates the skill.
        let activated = mgr.activate_conditional_for_paths(
            &[PathBuf::from("src/main.rs")],
            &dir,
        );
        assert!(activated.contains(&"rust-helper".to_string()));
        assert!(mgr.dynamic_skills.contains_key("rust-helper"));
    }

    #[test]
    fn slash_command_activates_conditional() {
        let dir = tempdir();
        let skills_dir = dir.join(".nonoclaw").join("skills");
        std::fs::create_dir_all(skills_dir.join("cond")).unwrap();
        std::fs::write(
            skills_dir.join("cond").join("SKILL.md"),
            "---\nname: cond\ndescription: c\npaths: [\"*.rs\"]\n---\nbody",
        )
        .unwrap();

        let mut mgr = SkillsManager::new(&dir);
        assert!(mgr.conditional_skills.contains_key("cond"));

        let ok = mgr.activate_slash_command("cond");
        assert!(ok);
        assert!(!mgr.conditional_skills.contains_key("cond"));
        assert!(mgr.dynamic_skills.contains_key("cond"));
    }

    #[test]
    fn render_prompt_includes_active_skills() {
        let dir = tempdir();
        let skills_dir = dir.join(".nonoclaw").join("skills");
        std::fs::create_dir_all(skills_dir.join("alpha")).unwrap();
        std::fs::write(
            skills_dir.join("alpha").join("SKILL.md"),
            "---\nname: alpha\ndescription: first skill\nwhen_to_use: when testing\n---\nAlpha body here.",
        )
        .unwrap();

        let mgr = SkillsManager::new(&dir);
        let prompt = mgr.render_prompt();
        assert!(prompt.contains("## alpha"));
        assert!(prompt.contains("first skill"));
        assert!(prompt.contains("when testing"));
        assert!(prompt.contains("Alpha body here."));
    }

    #[test]
    fn empty_render_prompt() {
        let dir = tempdir();
        let mgr = SkillsManager::new(&dir);
        // If the user has skills in ~/.nonoclaw/skills, render_prompt will be
        // non-empty. This is expected; the test just verifies that an empty
        // manager produces no prompt.
        if mgr.all_active().is_empty() {
            assert_eq!(mgr.render_prompt(), "");
        } else {
            // Non-empty → prompt must be valid (contain the skills).
            let prompt = mgr.render_prompt();
            assert!(!prompt.is_empty());
            assert!(prompt.contains("# Available Skills"));
        }
    }

    #[test]
    fn version_increments_on_change() {
        let dir = tempdir();
        let skills_dir = dir.join(".nonoclaw").join("skills");
        std::fs::create_dir_all(skills_dir.join("cond")).unwrap();
        std::fs::write(
            skills_dir.join("cond").join("SKILL.md"),
            "---\nname: cond\ndescription: c\npaths: [\"*.rs\"]\n---\nbody",
        )
        .unwrap();

        let mut mgr = SkillsManager::new(&dir);
        let v0 = mgr.version();
        mgr.activate_slash_command("cond");
        assert!(mgr.version() > v0);
    }

    #[test]
    fn trigger_matching() {
        assert!(triggers_match(
            &["deploy|ship|release".to_string()],
            "please deploy to prod"
        ));
        assert!(triggers_match(
            &["ship".to_string()],
            "can you ship this"
        ));
        assert!(!triggers_match(
            &["deploy".to_string()],
            "hello world"
        ));
        assert!(!triggers_match(&[], "deploy"));
    }

    #[test]
    fn trigger_activates_conditional_skill() {
        let dir = tempdir();
        let skills_dir = dir.join(".nonoclaw").join("skills");
        std::fs::create_dir_all(skills_dir.join("deployer")).unwrap();
        std::fs::write(
            skills_dir.join("deployer").join("SKILL.md"),
            "---\nname: deployer\ndescription: deploy\npaths: [\"deploy/**\"]\ntriggers: [\"deploy|ship|release\"]\n---\nDeploy instructions.",
        )
        .unwrap();

        let mut mgr = SkillsManager::new(&dir);
        assert!(mgr.conditional_skills.contains_key("deployer"));

        let matched = mgr.match_triggers("please deploy to prod");
        assert!(matched.contains(&"deployer".to_string()));
        assert!(!mgr.conditional_skills.contains_key("deployer"));
        assert!(mgr.dynamic_skills.contains_key("deployer"));
    }

    #[test]
    fn dynamic_discovery_from_subdir() {
        let dir = tempdir();
        // Create a nested .nonoclaw/skills in a subdirectory.
        let sub = dir.join("services").join("api");
        std::fs::create_dir_all(sub.join(".nonoclaw").join("skills").join("api-tools")).unwrap();
        std::fs::write(
            sub.join(".nonoclaw")
                .join("skills")
                .join("api-tools")
                .join("SKILL.md"),
            "---\nname: api-tools\ndescription: API helpers\n---\nAPI helper body.",
        )
        .unwrap();

        let mut mgr = SkillsManager::new(&dir);
        assert!(mgr.dynamic_skills.is_empty());

        // Simulate reading a file in the subdirectory.
        let discovered = mgr.discover_for_file_paths(
            &[sub.join("src").join("main.rs")],
            &dir,
        );
        assert!(!discovered.is_empty());
        assert!(mgr.dynamic_skills.contains_key("api-tools"));
    }

    #[test]
    fn dedup_dynamic_overrides_static() {
        let dir = tempdir();
        let skills_dir = dir.join(".nonoclaw").join("skills");
        std::fs::create_dir_all(skills_dir.join("dup")).unwrap();
        std::fs::write(
            skills_dir.join("dup").join("SKILL.md"),
            "---\nname: dup\ndescription: static version\n---\nstatic body",
        )
        .unwrap();

        let mut mgr = SkillsManager::new(&dir);
        assert!(mgr.static_skills.iter().any(|s| s.name == "dup"));

        // Add a dynamic skill with the same name.
        let dynamic = Skill {
            name: "dup".into(),
            description: "dynamic version".into(),
            body: "dynamic body".into(),
            source: String::new(),
            ..Default::default()
        };
        mgr.dynamic_skills.insert("dup".into(), dynamic);
        let active = mgr.all_active();
        // Dynamic "dup" overrides static "dup"; other static skills from
        // ~/.nonoclaw/skills may also be present.
        let dup = active.iter().find(|s| s.name == "dup").unwrap();
        assert_eq!(dup.description, "dynamic version");
    }

}
