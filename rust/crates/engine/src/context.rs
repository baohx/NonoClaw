//! Context gathering + system-prompt inputs. Mirrors `src/context.ts`
//! (`getSystemContext`, `getUserContext`), `src/utils/claudemd.ts`, and
//! `src/memdir/memdir.ts`.

use std::path::{Path, PathBuf};

/// Git snapshot taken at conversation start (mirrors `getSystemContext`).
#[derive(Debug, Clone, Default)]
pub struct SystemContext {
    pub git_summary: String,
}

/// User-injected context (mirrors `getUserContext`).
#[derive(Debug, Clone, Default)]
pub struct UserContext {
    pub nonoclaw_md: String,
    pub date: String,
    /// Content of `SYSTEM.md` if discovered. When present, it **completely
    /// replaces** the default BASE prompt body (identity + guidelines) —
    /// see pi's customPrompt pattern. Loaded from (in priority order):
    ///   1. `<cwd>/.nonoclaw/SYSTEM.md`
    ///   2. `~/.nonoclaw/SYSTEM.md`
    pub system_md_override: Option<String>,
    /// Content of `APPEND_SYSTEM.md` if discovered. Always appended to the
    /// end of Block 1, regardless of whether `system_md_override` is set.
    /// Loaded from the same locations as `SYSTEM.md`.
    pub append_system_md: Option<String>,
}

const GIT_STATUS_MAX: usize = 2000;
const LEGACY_MEMORY_MAX: usize = 50_000;
const PROJECT_OPEN: &str = "<project_context>\n";
const PROJECT_CLOSE: &str = "</project_context>\n";

/// Collect a git snapshot using the legacy compatibility cap.
pub async fn get_system_context(cwd: &Path) -> SystemContext {
    get_system_context_with_limit(cwd, usize::MAX).await
}

/// Collect a git snapshot with a hard character cap. The cap applies before
/// the snapshot reaches prompt assembly and is independent of provider cache
/// support.
pub async fn get_system_context_with_limit(cwd: &Path, max_chars: usize) -> SystemContext {
    if max_chars == 0 {
        return SystemContext::default();
    }
    let (branch, status, log, user) = tokio::join!(
        git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]),
        git_out(cwd, &["status"]),
        git_out(cwd, &["log", "--oneline", "-5"]),
        git_out(cwd, &["config", "user.name"]),
    );

    let mut summary = String::new();
    if !branch.is_empty() {
        summary.push_str(&format!("Current branch: {branch}\n"));
    }
    if !user.is_empty() {
        summary.push_str(&format!("Git user: {user}\n"));
    }
    if !status.is_empty() {
        let status = truncate_chars(status.trim(), GIT_STATUS_MAX.min(max_chars));
        summary.push_str(&format!("Git status:\n{status}\n"));
    }
    if !log.is_empty() {
        summary.push_str("Recent commits:\n");
        summary.push_str(log.trim());
        summary.push('\n');
    }
    SystemContext {
        git_summary: hard_limit_chars(&summary, max_chars),
    }
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

/// Gather project instructions without a practical compatibility limit.
pub fn get_user_context(cwd: &Path, add_dirs: &[PathBuf]) -> UserContext {
    get_user_context_with_limit(cwd, add_dirs, usize::MAX)
}

/// Gather project instructions under an exact character budget. Project-local
/// files and rules are processed before add-dir and user-global sources, so
/// mandatory repository instructions win when the budget is exhausted. Every
/// retained source keeps well-formed XML wrappers.
pub fn get_user_context_with_limit(
    cwd: &Path,
    add_dirs: &[PathBuf],
    project_max_chars: usize,
) -> UserContext {
    let mut nonoclaw_md = String::new();

    if let Some(content) = read_optional(&cwd.join(".nonoclaw/NONOCLAW.md")) {
        append_md_bounded(
            &mut nonoclaw_md,
            ".nonoclaw/NONOCLAW.md",
            content,
            project_max_chars,
        );
    }
    if let Some(content) = read_optional(&cwd.join(".nonoclaw/NONOCLAW.local.md")) {
        append_md_bounded(
            &mut nonoclaw_md,
            ".nonoclaw/NONOCLAW.local.md",
            content,
            project_max_chars,
        );
    }
    load_rules(
        &cwd.join(".nonoclaw/rules"),
        &mut nonoclaw_md,
        project_max_chars,
    );

    for directory in add_dirs {
        if let Some(content) = read_optional(&directory.join(".nonoclaw/NONOCLAW.md")) {
            append_md_bounded(
                &mut nonoclaw_md,
                &directory
                    .join(".nonoclaw/NONOCLAW.md")
                    .to_string_lossy()
                    .replace('\\', "/"),
                content,
                project_max_chars,
            );
        }
    }

    if let Some(home) = nonoclaw_core::nonoclaw_data_dir() {
        if let Some(content) = read_optional(&PathBuf::from(&home).join(".nonoclaw/NONOCLAW.md")) {
            append_md_bounded(
                &mut nonoclaw_md,
                "~/.nonoclaw/NONOCLAW.md",
                content,
                project_max_chars,
            );
        }
        load_rules(
            &PathBuf::from(&home).join(".nonoclaw/rules"),
            &mut nonoclaw_md,
            project_max_chars,
        );
    }

    let date = chrono::Local::now().format("%Y/%m/%d").to_string();
    close_project_context(&mut nonoclaw_md);

    // SYSTEM.md / APPEND_SYSTEM.md are bounded later as part of Block 1's
    // systemPromptTokens partition rather than the project-rules partition.
    let system_md_override = read_optional(&cwd.join(".nonoclaw/SYSTEM.md")).or_else(|| {
        nonoclaw_core::nonoclaw_data_dir()
            .and_then(|home| read_optional(&PathBuf::from(&home).join(".nonoclaw/SYSTEM.md")))
    });
    let append_system_md = read_optional(&cwd.join(".nonoclaw/APPEND_SYSTEM.md")).or_else(|| {
        nonoclaw_core::nonoclaw_data_dir().and_then(|home| {
            read_optional(&PathBuf::from(&home).join(".nonoclaw/APPEND_SYSTEM.md"))
        })
    });

    UserContext {
        nonoclaw_md,
        date,
        system_md_override,
        append_system_md,
    }
}

/// Scan `rules_dir/*.md`, sorted by filename, and append each under the shared
/// project-context budget.
fn load_rules(rules_dir: &Path, buf: &mut String, max_chars: usize) {
    let Ok(entries) = std::fs::read_dir(rules_dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("md"))
        .collect();
    paths.sort();
    for path in &paths {
        let relative = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rule.md");
        if let Some(content) = read_optional(path) {
            append_md_bounded(buf, &format!("rules/{relative}"), content, max_chars);
        }
    }
}

fn append_md_bounded(buf: &mut String, source: &str, content: String, max_chars: usize) {
    let opening = if buf.is_empty() { PROJECT_OPEN } else { "" };
    let prefix = format!("<project_instructions path=\"{source}\">\n");
    let suffix = "\n</project_instructions>\n";
    let fixed_chars = opening.chars().count()
        + prefix.chars().count()
        + suffix.chars().count()
        + PROJECT_CLOSE.chars().count();
    let used_chars = buf.chars().count();
    if fixed_chars > max_chars.saturating_sub(used_chars) {
        return;
    }
    let content_chars = max_chars - used_chars - fixed_chars;
    if content_chars == 0 && !content.is_empty() {
        return;
    }
    buf.push_str(opening);
    buf.push_str(&prefix);
    buf.push_str(&hard_limit_chars(&content, content_chars));
    buf.push_str(suffix);
}

fn close_project_context(buf: &mut String) {
    if buf.starts_with(PROJECT_OPEN) && !buf.ends_with(PROJECT_CLOSE) {
        buf.push_str(PROJECT_CLOSE);
    }
}

/// Load memory with the legacy 50K-character compatibility cap.
pub fn load_memory_prompt(cwd: &Path) -> Option<String> {
    load_memory_prompt_with_limit(cwd, LEGACY_MEMORY_MAX)
}

/// Load memory in importance-first order under a hard character limit.
/// The single `max_chars` budget is split into the legacy proportions
/// (beads 20%, facts 20%, wiki 10%, index 50% — matching the pre-partition
/// hard-coded 20K/20K/5K/25K allocation under the 50K cap).
pub fn load_memory_prompt_with_limit(cwd: &Path, max_chars: usize) -> Option<String> {
    let max_chars = max_chars.min(LEGACY_MEMORY_MAX);
    if max_chars == 0 {
        return None;
    }
    load_memory_prompt_with_partitions(
        cwd,
        max_chars * 2 / 10,
        max_chars * 2 / 10,
        max_chars / 10,
        max_chars * 5 / 10,
        max_chars,
    )
}

/// Load memory under **independent per-partition budgets**, so a huge wiki
/// index can never crowd out facts, or a fact dump the beads. Partitions:
///
/// - `beads_chars` — active-task beads (task continuity)
/// - `facts_chars` — importance-ranked facts
/// - `wiki_chars` — LLM-Wiki index preview
/// - `index_chars` — legacy MEMORY.md + per-file entries
/// - `total_chars` — hard cap over the whole rendered block (≤ 50K legacy cap)
pub fn load_memory_prompt_with_partitions(
    cwd: &Path,
    beads_chars: usize,
    facts_chars: usize,
    wiki_chars: usize,
    index_chars: usize,
    total_chars: usize,
) -> Option<String> {
    let total_chars = total_chars.min(LEGACY_MEMORY_MAX);
    if total_chars == 0 {
        return None;
    }
    let mem_dir = cwd.join(".nonoclaw/memory");
    if !mem_dir.is_dir() {
        return None;
    }

    let mut buf = String::new();
    let beads = nonoclaw_tools::memory::load_beads(cwd);
    let active: Vec<&nonoclaw_tools::memory::Bead> =
        nonoclaw_tools::memory::active_beads(&beads)
            .into_iter()
            .take(5)
            .collect();
    let facts = nonoclaw_tools::memory::load_facts(cwd);
    let mut top_facts: Vec<&nonoclaw_tools::memory::Fact> = facts.iter().collect();
    top_facts.sort_by(|left, right| {
        right
            .importance
            .partial_cmp(&left.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    top_facts.truncate(10);

    if !active.is_empty() || !top_facts.is_empty() {
        let context = nonoclaw_tools::memory::render_memory_partitioned(
            &active,
            &top_facts,
            beads_chars,
            facts_chars,
        );
        if !context.is_empty() {
            buf.push_str(&context);
            buf.push_str("\n---\n\n");
        }
    }

    if buf.chars().count() < total_chars && wiki_chars > 0 {
        if let Some(wiki_index) = nonoclaw_tools::memory::load_wiki_index(cwd) {
            let preview = truncate_chars(&wiki_index, wiki_chars);
            buf.push_str("## Knowledge Base (Wiki Index)\n\n");
            buf.push_str(&preview);
            buf.push_str("\n\n---\n\n");
        }
    }

    if buf.chars().count() < total_chars && index_chars > 0 {
        // MEMORY.md and per-file entries share the `index_chars` partition.
        let mut index_used = 0usize;
        let index_path = mem_dir.join("MEMORY.md");
        if let Some(index) = read_optional(&index_path) {
            let trimmed = truncate_chars(&index, index_chars);
            let lines: Vec<&str> = trimmed.lines().take(200).collect();
            if !lines.is_empty() {
                let block = format!("{}\n\n", lines.join("\n"));
                let block_chars = block.chars().count();
                if index_used + block_chars <= index_chars {
                    buf.push_str(&block);
                    index_used += block_chars;
                } else if index_used < index_chars {
                    buf.push_str(&truncate_chars(&block, index_chars - index_used));
                    index_used = index_chars;
                }
            }
        }

        if index_used < index_chars {
            if let Ok(entries) = std::fs::read_dir(&mem_dir) {
                let mut paths: Vec<std::path::PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .and_then(|extension| extension.to_str())
                            .map(|extension| extension == "md")
                            .unwrap_or(false)
                            && path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .map(|name| name != "MEMORY.md")
                                .unwrap_or(false)
                    })
                    .collect();
                paths.sort();
                for path in &paths {
                    if buf.chars().count() >= total_chars || index_used >= index_chars {
                        break;
                    }
                    if let Some(content) = read_optional(path) {
                        let fact = strip_frontmatter(&content);
                        if !fact.trim().is_empty() {
                            let name = path
                                .file_stem()
                                .and_then(|name| name.to_str())
                                .unwrap_or("fact");
                            let line = format!("**{name}**: {fact}\n\n");
                            let line_chars = line.chars().count();
                            if index_used + line_chars > index_chars {
                                break;
                            }
                            buf.push_str(&line);
                            index_used += line_chars;
                        }
                    }
                }
            }
        }
    }

    let trimmed = buf.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(hard_limit_chars(trimmed, total_chars))
    }
}

fn hard_limit_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        value.chars().take(max_chars).collect()
    }
}

/// Strip YAML frontmatter (`---\n...\n---\n`) from a string, returning the
/// body text that follows. If no frontmatter is present, returns the original.
pub fn strip_frontmatter(s: &str) -> String {
    let s = s.trim();
    if !s.starts_with("---") {
        return s.to_string();
    }
    // Find the second `---` delimiter.
    let after_first = &s[3..]; // skip opening ---
    if let Some(pos) = after_first.find("\n---") {
        let body = after_first[pos + 4..].trim();
        body.to_string()
    } else {
        // Malformed frontmatter — return as-is.
        s.to_string()
    }
}

fn read_optional(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n... [truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_works() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        let big = "x".repeat(20);
        let t = truncate_chars(&big, 5);
        assert!(t.contains("truncated"));
        assert!(t.starts_with("xxxxx"));
    }

    #[test]
    fn memory_partitions_are_independent() {
        let root = std::env::temp_dir().join(format!(
            "nonoclaw-memory-partitions-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join(".nonoclaw/memory")).unwrap();
        std::fs::create_dir_all(root.join(".nonoclaw/wiki")).unwrap();
        // A huge MEMORY.md index + huge wiki index that would crowd out the
        // facts section if the budget were shared.
        std::fs::write(
            root.join(".nonoclaw/memory/MEMORY.md"),
            format!("HUGELINE {}\n", "z".repeat(5000)),
        )
        .unwrap();
        let fact_a = nonoclaw_tools::memory::Fact {
            name: "fact-a".into(),
            title: "Fact A".into(),
            content: "Fact A body content that must survive a tiny wiki/index budget.".into(),
            fact_type: nonoclaw_tools::memory::FactType::General,
            importance: 0.9,
            confidence: 0.9,
            created: String::new(),
            updated: String::new(),
            sources: vec![],
            supersedes: None,
            tags: vec![],
        };
        fact_a.save(&root).unwrap();
        std::fs::write(
            root.join(".nonoclaw/wiki/index.md"),
            "Wiki index line ".repeat(2000),
        )
        .unwrap();

        // Facts partition gets its own budget even when wiki/index are tiny.
        let loaded = load_memory_prompt_with_partitions(&root, 200, 4000, 50, 50, 20_000)
            .unwrap_or_default();
        assert!(
            loaded.contains("Fact A"),
            "facts partition must survive a tiny wiki/index budget: {}",
            &loaded[..loaded.len().min(120)]
        );

        // And beads-only budget cannot be flooded by facts: with facts budget
        // 0 the facts section is absent while beads still render.
        let bead = nonoclaw_tools::memory::Bead {
            id: "bead-1".into(),
            title: "Fix timeout".into(),
            status: nonoclaw_tools::memory::BeadStatus::InProgress,
            priority: 8,
            created: String::new(),
            updated: String::new(),
            session: "sess-1".into(),
            content: "Investigating login timeout.".into(),
        };
        bead.save(&root).unwrap();
        let no_facts =
            load_memory_prompt_with_partitions(&root, 400, 0, 0, 0, 20_000).unwrap_or_default();
        assert!(
            no_facts.contains("Fix timeout") && !no_facts.contains("Fact A"),
            "beads must render under their own partition: {}",
            &no_facts[..no_facts.len().min(120)]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_memory_limit_preserves_behaviour() {
        // The single-limit wrapper still caps total size at `max_chars`.
        let root = std::env::temp_dir().join(format!(
            "nonoclaw-memory-legacy-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join(".nonoclaw/memory")).unwrap();
        let fact_b = nonoclaw_tools::memory::Fact {
            name: "fact-b".into(),
            title: "Fact B".into(),
            content: "b".repeat(2000),
            fact_type: nonoclaw_tools::memory::FactType::General,
            importance: 0.5,
            confidence: 0.5,
            created: String::new(),
            updated: String::new(),
            sources: vec![],
            supersedes: None,
            tags: vec![],
        };
        fact_b.save(&root).unwrap();
        let loaded = load_memory_prompt_with_limit(&root, 512);
        assert!(loaded.is_some());
        assert!(loaded.unwrap().chars().count() <= 512);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn user_context_date_is_set() {
        let uc = get_user_context(Path::new("/nonexistent"), &[]);
        assert!(!uc.date.is_empty());
    }

    #[test]
    fn project_context_budget_preserves_local_rules_and_xml_boundaries() {
        let root =
            std::env::temp_dir().join(format!("nonoclaw-context-budget-{}", uuid::Uuid::new_v4()));
        let added = root.join("added");
        std::fs::create_dir_all(root.join(".nonoclaw")).unwrap();
        std::fs::create_dir_all(added.join(".nonoclaw")).unwrap();
        std::fs::write(
            root.join(".nonoclaw/NONOCLAW.md"),
            format!("LOCAL_PRIORITY {}", "x".repeat(1000)),
        )
        .unwrap();
        std::fs::write(
            added.join(".nonoclaw/NONOCLAW.md"),
            "LOWER_PRIORITY_ADD_DIR",
        )
        .unwrap();

        let context = get_user_context_with_limit(&root, &[added], 180).nonoclaw_md;
        assert!(context.chars().count() <= 180);
        assert!(context.starts_with(PROJECT_OPEN));
        assert!(context.ends_with(PROJECT_CLOSE));
        assert!(context.contains("LOCAL_PRIORITY"));
        assert!(!context.contains("LOWER_PRIORITY_ADD_DIR"));
        assert!(context.contains("</project_instructions>"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn memory_loader_applies_exact_hard_limit() {
        let root =
            std::env::temp_dir().join(format!("nonoclaw-memory-budget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".nonoclaw/memory")).unwrap();
        std::fs::write(
            root.join(".nonoclaw/memory/MEMORY.md"),
            "important-memory ".repeat(100),
        )
        .unwrap();
        let memory = load_memory_prompt_with_limit(&root, 64).unwrap();
        assert!(memory.chars().count() <= 64);
        assert!(memory.starts_with("important-memory"));
        let _ = std::fs::remove_dir_all(root);
    }

    // ========================================================================
    // Batch 4 — XML structured context wrapping
    // ========================================================================

    #[test]
    fn nonoclaw_md_uses_project_context_xml() {
        use std::io::Write;
        // Set up a tempdir with a .nonoclaw/NONOCLAW.md
        let tmp = std::env::temp_dir().join(format!("nc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".nonoclaw")).unwrap();
        let mut f = std::fs::File::create(tmp.join(".nonoclaw/NONOCLAW.md")).unwrap();
        writeln!(f, "project rules here").unwrap();
        drop(f);

        let uc = get_user_context(&tmp, &[]);
        assert!(
            uc.nonoclaw_md.contains("<project_context>"),
            "must open <project_context>: got\n{}",
            uc.nonoclaw_md
        );
        assert!(
            uc.nonoclaw_md.contains("</project_context>"),
            "must close </project_context>: got\n{}",
            uc.nonoclaw_md
        );
        assert!(
            uc.nonoclaw_md
                .contains("<project_instructions path=\".nonoclaw/NONOCLAW.md\">"),
            "must use <project_instructions> with path attr: got\n{}",
            uc.nonoclaw_md
        );
        assert!(
            uc.nonoclaw_md.contains("</project_instructions>"),
            "must close <project_instructions>"
        );
        assert!(
            uc.nonoclaw_md.contains("project rules here"),
            "must embed the file content"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn nonoclaw_md_empty_when_no_files() {
        let tmp = std::env::temp_dir().join(format!("nc_test_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let uc = get_user_context(&tmp, &[]);
        assert!(
            uc.nonoclaw_md.is_empty(),
            "no NONOCLAW.md → empty string, got\n{}",
            uc.nonoclaw_md
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ========================================================================
    // Batch 6 — SYSTEM.md / APPEND_SYSTEM.md discovery
    // ========================================================================

    #[test]
    fn system_md_loaded_from_project_dir() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("nc_test_sysmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".nonoclaw")).unwrap();
        let mut f = std::fs::File::create(tmp.join(".nonoclaw/SYSTEM.md")).unwrap();
        writeln!(f, "Custom system prompt body").unwrap();
        drop(f);

        let uc = get_user_context(&tmp, &[]);
        assert_eq!(
            uc.system_md_override.as_deref().map(str::trim),
            Some("Custom system prompt body"),
            "SYSTEM.md must be loaded into system_md_override"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn append_system_md_loaded_from_project_dir() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("nc_test_appmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".nonoclaw")).unwrap();
        let mut f = std::fs::File::create(tmp.join(".nonoclaw/APPEND_SYSTEM.md")).unwrap();
        writeln!(f, "extra instructions").unwrap();
        drop(f);

        let uc = get_user_context(&tmp, &[]);
        assert_eq!(
            uc.append_system_md.as_deref().map(str::trim),
            Some("extra instructions"),
            "APPEND_SYSTEM.md must be loaded into append_system_md"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_system_files_yield_none() {
        let tmp = std::env::temp_dir().join(format!("nc_test_nofiles_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let uc = get_user_context(&tmp, &[]);
        assert!(uc.system_md_override.is_none());
        assert!(uc.append_system_md.is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
