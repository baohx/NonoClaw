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
}

const GIT_STATUS_MAX: usize = 2000;

/// Collect a git snapshot for the system prompt. Runs git as a subprocess;
/// fails quietly (returns empty) outside a repo.
pub async fn get_system_context(cwd: &Path) -> SystemContext {
    let branch = git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let status = git_out(cwd, &["status"]).await;
    let log = git_out(cwd, &["log", "--oneline", "-5"]).await;
    let user = git_out(cwd, &["config", "user.name"]).await;

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
                &d.join(".nonoclaw/NONOCLAW.md").display().to_string(),
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
    UserContext { nonoclaw_md, date }
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
        buf.push_str("# Project context (NONOCLAW.md)\n\n");
    }
    buf.push_str(&format!("## from {source}\n\n{content}\n\n"));
}

/// Load the memory index (`.nonoclaw/memory/MEMORY.md`), capped at 200 lines /
/// 25 KB. Mirrors `loadMemoryPrompt`.
pub fn load_memory_prompt(cwd: &Path) -> Option<String> {
    let path = cwd.join(".nonoclaw/memory/MEMORY.md");
    let content = read_optional(&path)?;
    let trimmed = truncate_chars(&content, 25_000);
    let lines: Vec<&str> = trimmed.lines().take(200).collect();
    Some(lines.join("\n"))
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
}
