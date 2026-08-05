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

/// Collect a git snapshot for the system prompt. Runs git as a subprocess;
/// fails quietly (returns empty) outside a repo. The four git commands are
/// spawned in parallel — they're independent and the latency win is real
/// (4 × ~10ms sequential → ~10ms wall time).
pub async fn get_system_context(cwd: &Path) -> SystemContext {
    let (branch, status, log, user) = tokio::join!(
        git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]),
        git_out(cwd, &["status"]),
        git_out(cwd, &["log", "--oneline", "-5"]),
        git_out(cwd, &["config", "user.name"]),
    );

    let mut s = String::new();
    if !branch.is_empty() {
        s.push_str(&format!("Current branch: {branch}\n"));
    }
    if !user.is_empty() {
        s.push_str(&format!("Git user: {user}\n"));
    }
    if !status.is_empty() {
        let status = truncate_chars(status.trim(), GIT_STATUS_MAX);
        s.push_str(&format!("Git status:\n{status}\n"));
    }
    if !log.is_empty() {
        s.push_str("Recent commits:\n");
        s.push_str(log.trim());
        s.push('\n');
    }
    SystemContext { git_summary: s }
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

/// Gather NONOCLAW.md content + current date.
///
/// Loading order (each source appended in sequence):
///   1. project `<cwd>/.nonoclaw/NONOCLAW.md`
///   2. project `<cwd>/.nonoclaw/NONOCLAW.local.md` (gitignored, local-only)
///   3. project `<cwd>/.nonoclaw/rules/*.md`       (alphabetically sorted)
///   4. each `--add-dir/.nonoclaw/NONOCLAW.md`
///   5. user   `~/.nonoclaw/NONOCLAW.md`
///   6. user   `~/.nonoclaw/rules/*.md`
pub fn get_user_context(cwd: &Path, add_dirs: &[PathBuf]) -> UserContext {
    let mut nonoclaw_md = String::new();

    // 1. Project NONOCLAW.md
    if let Some(content) = read_optional(&cwd.join(".nonoclaw/NONOCLAW.md")) {
        append_md(&mut nonoclaw_md, ".nonoclaw/NONOCLAW.md", content);
    }
    // 2. Project NONOCLAW.local.md (gitignored)
    if let Some(content) = read_optional(&cwd.join(".nonoclaw/NONOCLAW.local.md")) {
        append_md(&mut nonoclaw_md, ".nonoclaw/NONOCLAW.local.md", content);
    }
    // 3. Project rules/*.md
    load_rules(&cwd.join(".nonoclaw/rules"), &mut nonoclaw_md);

    // 4. --add-dir NONOCLAW.md files
    for d in add_dirs {
        if let Some(content) = read_optional(&d.join(".nonoclaw/NONOCLAW.md")) {
            append_md(
                &mut nonoclaw_md,
                &d.join(".nonoclaw/NONOCLAW.md").to_string_lossy().replace('\\', "/"),
                content,
            );
        }
    }

    // 5-6. User-global
    if let Some(home) = nonoclaw_core::nonoclaw_data_dir() {
        // 5. User NONOCLAW.md
        if let Some(content) = read_optional(&PathBuf::from(&home).join(".nonoclaw/NONOCLAW.md")) {
            append_md(&mut nonoclaw_md, "~/.nonoclaw/NONOCLAW.md", content);
        }
        // 6. User rules/*.md
        load_rules(
            &PathBuf::from(&home).join(".nonoclaw/rules"),
            &mut nonoclaw_md,
        );
    }

    let date = chrono::Local::now().format("%Y/%m/%d").to_string();
    close_project_context(&mut nonoclaw_md);

    // SYSTEM.md / APPEND_SYSTEM.md discovery. Project-local file wins over
    // the user-global one; only the first found is used. These are loaded
    // raw (no XML wrapping) — they're prompt-body content, not context.
    let system_md_override = read_optional(&cwd.join(".nonoclaw/SYSTEM.md")).or_else(|| {
        nonoclaw_core::nonoclaw_data_dir().and_then(|home| {
            read_optional(&PathBuf::from(&home).join(".nonoclaw/SYSTEM.md"))
        })
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

/// Scan `rules_dir/*.md`, sorted by filename, and append each to `buf`.
fn load_rules(rules_dir: &Path, buf: &mut String) {
    let Ok(entries) = std::fs::read_dir(rules_dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();
    for p in &paths {
        let rel = p.file_name().and_then(|n| n.to_str()).unwrap_or("rule.md");
        if let Some(content) = read_optional(p) {
            append_md(buf, &format!("rules/{rel}"), content);
        }
    }
}

fn append_md(buf: &mut String, source: &str, content: String) {
    if buf.is_empty() {
        buf.push_str("<project_context>\n");
    }
    buf.push_str(&format!(
        "<project_instructions path=\"{source}\">\n{content}\n</project_instructions>\n"
    ));
}

/// Close the `<project_context>` wrapper opened by the first `append_md` call.
/// Idempotent: only appends the closing tag when the buffer actually opened
/// one. Must be called once after all `append_md`/`load_rules` calls, before
/// returning the assembled NONOCLAW.md context.
fn close_project_context(buf: &mut String) {
    if buf.starts_with("<project_context>\n") && !buf.ends_with("</project_context>\n") {
        buf.push_str("</project_context>\n");
    }
}

/// Load the memory index + individual fact files from `.nonoclaw/memory/`.
///
/// Loads:
/// 1. `MEMORY.md` — the index (25 KB / 200 line cap)
/// 2. Individual `.md` fact files (excluding `MEMORY.md`) — each file is one
///    memory fact. Files with YAML frontmatter have it stripped; the body text
///    is what the model sees.
///
/// Total output capped at ~50 KB. Returns `None` if the memory directory doesn't
/// exist or contains nothing.
pub fn load_memory_prompt(cwd: &Path) -> Option<String> {
    let mem_dir = cwd.join(".nonoclaw/memory");
    if !mem_dir.is_dir() {
        return None;
    }

    let mut buf = String::new();

    // 0. Active beads + important facts (cross-session memory)
    let beads = nonoclaw_tools::memory::load_beads(cwd);
    let active: Vec<&nonoclaw_tools::memory::Bead> = nonoclaw_tools::memory::active_beads(&beads)
        .into_iter()
        .take(5)
        .collect();
    let facts = nonoclaw_tools::memory::load_facts(cwd);
    let mut top_facts: Vec<&nonoclaw_tools::memory::Fact> = facts.iter().collect();
    top_facts.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    top_facts.truncate(10);

    if !active.is_empty() || !top_facts.is_empty() {
        let ctx = nonoclaw_tools::memory::render_memory_context(&active, &top_facts, 20_000);
        if !ctx.is_empty() {
            buf.push_str(&ctx);
            buf.push_str("\n---\n\n");
        }
    }

    // 0.5 Wiki index (LLM Wiki knowledge base)
    if let Some(wiki_index) = nonoclaw_tools::memory::load_wiki_index(cwd) {
        let preview = truncate_chars(&wiki_index, 5000);
        buf.push_str("## Knowledge Base (Wiki Index)\n\n");
        buf.push_str(&preview);
        buf.push_str("\n\n---\n\n");
    }

    // 1. MEMORY.md index
    let index_path = mem_dir.join("MEMORY.md");
    if let Some(index) = read_optional(&index_path) {
        let trimmed = truncate_chars(&index, 25_000);
        let lines: Vec<&str> = trimmed.lines().take(200).collect();
        if !lines.is_empty() {
            buf.push_str(&lines.join("\n"));
            buf.push_str("\n\n");
        }
    }

    // 2. Individual fact files
    if let Ok(entries) = std::fs::read_dir(&mem_dir) {
        let mut paths: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e == "md")
                    .unwrap_or(false)
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n != "MEMORY.md")
                        .unwrap_or(false)
            })
            .collect();
        paths.sort();
        for p in &paths {
            if let Some(content) = read_optional(p) {
                let fact = strip_frontmatter(&content);
                if !fact.trim().is_empty() {
                    let name = p.file_stem().and_then(|n| n.to_str()).unwrap_or("fact");
                    buf.push_str(&format!("**{name}**: {fact}\n\n"));
                }
            }
        }
    }

    let trimmed = buf.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(truncate_chars(&trimmed, 50_000))
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
    fn user_context_date_is_set() {
        let uc = get_user_context(Path::new("/nonexistent"), &[]);
        assert!(!uc.date.is_empty());
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
        assert!(uc.nonoclaw_md.contains("<project_context>"),
                "must open <project_context>: got\n{}", uc.nonoclaw_md);
        assert!(uc.nonoclaw_md.contains("</project_context>"),
                "must close </project_context>: got\n{}", uc.nonoclaw_md);
        assert!(uc.nonoclaw_md.contains("<project_instructions path=\".nonoclaw/NONOCLAW.md\">"),
                "must use <project_instructions> with path attr: got\n{}", uc.nonoclaw_md);
        assert!(uc.nonoclaw_md.contains("</project_instructions>"),
                "must close <project_instructions>");
        assert!(uc.nonoclaw_md.contains("project rules here"),
                "must embed the file content");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn nonoclaw_md_empty_when_no_files() {
        let tmp = std::env::temp_dir().join(format!("nc_test_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let uc = get_user_context(&tmp, &[]);
        assert!(uc.nonoclaw_md.is_empty(),
                "no NONOCLAW.md → empty string, got\n{}", uc.nonoclaw_md);
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
