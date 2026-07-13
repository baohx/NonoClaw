//! Session persistence + resume. Mirrors the role of `src/history.ts` and the
//! `~/.nonoclaw/projects/<encoded-cwd>/<uuid>.jsonl` layout.
//!
//! Each session is a JSONL file under
//! `~/.nonoclaw/projects/<sanitized-cwd>/sessions/<id>.jsonl` (override the root
//! with `NONOCLAW_HOME`). The first line is a `session` metadata entry;
//! subsequent lines are `message` entries (the transcript) or optional
//! `summary` entries.

use std::path::{Path, PathBuf};

use nonoclaw_core::Message;
use serde::{Deserialize, Serialize};

/// One JSONL line in a session file.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SessionEntry {
    Session {
        id: String,
        cwd: String,
        model: String,
        started: String,
    },
    Message(Message),
    Summary {
        text: String,
    },
    CustomTitle {
        title: String,
    },
    AiTitle {
        title: String,
    },
    LastPrompt {
        prompt: String,
    },
    Tag {
        tag: String,
    },
    Mode {
        mode: String,
    },
}

/// Metadata for a discovered session (for `--list-sessions`).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub started: Option<String>,
    pub message_count: usize,
    pub summary: String,
    pub title: Option<String>,
    pub tag: Option<String>,
    pub mtime: std::time::SystemTime,
}

/// Write a custom title entry for the session.
pub fn write_custom_title(path: &Path, title: &str) -> std::io::Result<()> {
    append_entry(path, &SessionEntry::CustomTitle { title: title.to_string() })
}

/// Write an AI-generated title.
pub fn write_ai_title(path: &Path, title: &str) -> std::io::Result<()> {
    append_entry(path, &SessionEntry::AiTitle { title: title.to_string() })
}

/// Write the first user prompt as a last-prompt entry.
pub fn write_last_prompt(path: &Path, prompt: &str) -> std::io::Result<()> {
    append_entry(path, &SessionEntry::LastPrompt { prompt: prompt.to_string() })
}

/// Get the best available session title.
pub fn get_session_title(path: &Path) -> Option<String> {
    let entries = read_entries(path)?;
    let mut custom = None;
    let mut ai = None;
    let mut last = None;
    for e in &entries {
        match e {
            SessionEntry::CustomTitle { title } => custom = Some(title.clone()),
            SessionEntry::AiTitle { title } => ai = Some(title.clone()),
            SessionEntry::LastPrompt { prompt } => last = Some(prompt.chars().take(200).collect()),
            _ => {}
        }
    }
    custom.or(ai).or(last)
}

/// Scan session entries for metadata (titles, tags). Only reads head+tail.
fn read_entries(path: &Path) -> Option<Vec<SessionEntry>> {
    let content = std::fs::read_to_string(path).ok()?;
    let entries: Vec<SessionEntry> = content
        .lines()
        .filter_map(|line| serde_json::from_str::<SessionEntry>(line).ok())
        .collect();
    Some(entries)
}

fn append_entry(path: &Path, entry: &SessionEntry) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path)?;
    let line = serde_json::to_string(entry)?;
    writeln!(f, "{line}")
}

/// Resolve the root directory for session storage (`$NONOCLAW_HOME` or `~/.nonoclaw`).
pub fn home_root() -> Option<PathBuf> {
    nonoclaw_core::nonoclaw_data_dir()
}

/// The per-project directory holding that cwd's sessions.
pub fn project_dir(cwd: &Path) -> Option<PathBuf> {
    let root = home_root()?;
    let sanitized = sanitize_cwd(cwd);
    Some(root.join("projects").join(sanitized))
}

/// The path of a specific session's JSONL file.
pub fn session_path(cwd: &Path, id: &str) -> Option<PathBuf> {
    Some(
        project_dir(cwd)?
            .join("sessions")
            .join(format!("{id}.jsonl")),
    )
}

/// Replace path separators so an absolute cwd becomes a single path segment.
fn sanitize_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-")
}

/// Generate a new session id.
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Write the metadata header for a fresh session (no-op if it already exists).
pub fn write_header(
    path: &Path,
    id: &str,
    cwd: &Path,
    model: &str,
    started: &str,
) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let entry = SessionEntry::Session {
        id: id.to_string(),
        cwd: cwd.to_string_lossy().to_string(),
        model: model.to_string(),
        started: started.to_string(),
    };
    let line = serde_json::to_string(&entry)?;
    append_line(path, &line)
}

/// Append a transcript message to the session file.
pub fn append_message(path: &Path, msg: &Message) -> std::io::Result<()> {
    let entry = SessionEntry::Message(msg.clone());
    let line = serde_json::to_string(&entry)?;
    append_line(path, &line)
}

/// Truncate a session file back to just the header line (for `/clear`).
pub fn clear_session(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let (_started, _summary, _messages) = load_session(path)?;
    // Reconstruct the header from what we loaded, then overwrite.
    // If load failed, just truncate the file.
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Some(header_line) = text.lines().next() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(header_line) {
                if v.get("kind").and_then(|k| k.as_str()) == Some("session") {
                    let line = serde_json::to_string(&v).unwrap_or_default();
                    std::fs::write(path, format!("{line}\n"))?;
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Load a session file: returns (started, summary, messages).
pub fn load_session(path: &Path) -> std::io::Result<(Option<String>, String, Vec<Message>)> {
    let text = std::fs::read_to_string(path)?;
    let mut started = None;
    let mut summary = String::new();
    let mut messages = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("kind").and_then(|k| k.as_str()) {
            Some("session") => {
                started = v
                    .get("started")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string());
            }
            Some("summary") => {
                if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                    summary = t.to_string();
                }
            }
            Some("message") => {
                if let Ok(m) = serde_json::from_value::<Message>(v.clone()) {
                    messages.push(m);
                }
            }
            _ => {}
        }
    }
    Ok((started, summary, messages))
}

/// List sessions for a cwd, most-recent first.
pub fn list_sessions(cwd: &Path) -> std::io::Result<Vec<SessionInfo>> {
    let Some(dir) = project_dir(cwd) else {
        return Ok(Vec::new());
    };
    let sessions_dir = dir.join("sessions");
    let mut out = Vec::new();
    let read = match std::fs::read_dir(&sessions_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    for entry in read {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = entry
            .metadata()?
            .modified()
            .unwrap_or(std::time::UNIX_EPOCH);
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let (started, summary, messages) =
            load_session(&path).unwrap_or((None, String::new(), Vec::new()));
        let summary = if !summary.is_empty() {
            summary
        } else {
            // Fall back to the first user message text.
            messages
                .iter()
                .find_map(|m| match &m.content {
                    nonoclaw_core::MessageContent::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        out.push(SessionInfo {
            id,
            started,
            message_count: messages.len(),
            summary,
            title: None,
            tag: None,
            mtime,
        });
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    Ok(out)
}

/// Find the most recently modified session id for a cwd (for `--continue`).
pub fn most_recent_session(cwd: &Path) -> std::io::Result<Option<String>> {
    Ok(list_sessions(cwd)?.into_iter().next().map(|s| s.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_core::{Message, MessageContent, Role};

    #[test]
    fn sanitize_handles_absolute_paths() {
        assert_eq!(
            sanitize_cwd(Path::new("/home/baohx/NonoClaw")),
            "home-baohx-NonoClaw"
        );
    }

    #[test]
    fn write_load_roundtrip() {
        let tmp = tempdir();
        let path = tmp.join("s.jsonl");
        write_header(
            &path,
            "id-1",
            Path::new("/proj"),
            "model-x",
            "2026-07-07T10:00:00Z",
        )
        .unwrap();
        let m1 = Message::user(MessageContent::from_text("hello"));
        let m2 = Message::assistant(MessageContent::from_text("hi there"));
        append_message(&path, &m1).unwrap();
        append_message(&path, &m2).unwrap();

        let (started, summary, messages) = load_session(&path).unwrap();
        assert_eq!(started.as_deref(), Some("2026-07-07T10:00:00Z"));
        assert!(summary.is_empty());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
    }

    #[test]
    fn list_returns_most_recent_first() {
        let root = tempdir().join("projroot");
        std::env::set_var("NONOCLAW_HOME", &root);
        let cwd = Path::new("/proj");
        let p1 = session_path(cwd, "older").unwrap();
        let p2 = session_path(cwd, "newer").unwrap();
        write_header(&p1, "older", cwd, "m", "t1").unwrap();
        append_message(&p1, &Message::user(MessageContent::from_text("old"))).unwrap();
        // small delay so p2 mtime >= p1 mtime
        std::thread::sleep(std::time::Duration::from_millis(200));
        write_header(&p2, "newer", cwd, "m", "t2").unwrap();
        append_message(&p2, &Message::user(MessageContent::from_text("new"))).unwrap();

        let list = list_sessions(cwd).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "newer");
        assert_eq!(list[1].id, "older");
        assert_eq!(list[0].message_count, 1);
        std::env::remove_var("NONOCLAW_HOME");
    }

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("nonoclaw-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
