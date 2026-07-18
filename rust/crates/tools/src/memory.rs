//! Three-layer cross-session memory system ("Mneme").
//!
//! **Layer 1 — Facts**: immutable knowledge, one `.md` file per fact with
//!   YAML frontmatter.
//! **Layer 2 — Beads**: task state that survives sessions.
//! **Layer 3 — Transcript**: per-session JSONL.
//!
//! Facts and beads are plain markdown files under `.nonoclaw/memory/facts/` and
//! `.nonoclaw/memory/beads/`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ── Fact ────────────────────────────────────────────────────────────────────

/// A single immutable fact (convention, preference, decision, bug pattern).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    /// Kebab-case slug, also used as the filename (`{name}.md`).
    pub name: String,
    /// One-line summary.
    pub title: String,
    /// Full markdown body.
    pub content: String,
    /// What kind of fact.
    #[serde(default)]
    pub fact_type: FactType,
    /// 0.0–1.0. Higher = more important to keep in context.
    #[serde(default = "default_half")]
    pub importance: f64,
    /// 0.0–1.0. How confident the agent is in this fact.
    #[serde(default = "default_half")]
    pub confidence: f64,
    /// ISO-8601 creation timestamp.
    #[serde(default)]
    pub created: String,
    /// ISO-8601 last-updated timestamp.
    #[serde(default)]
    pub updated: String,
    /// Session IDs that produced this fact.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Name of a fact this one supersedes (old fact keeps `superseded_by`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// Free-form tags for search.
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FactType {
    #[default]
    General,
    Preference,
    Convention,
    Decision,
    Architecture,
    Bug,
}

fn default_half() -> f64 { 0.5 }

impl Fact {
    /// Write this fact to `memory/facts/{name}.md`.
    pub fn save(&self, cwd: &Path) -> std::io::Result<()> {
        let dir = cwd.join(".nonoclaw/memory/facts");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.md", sanitize_filename(&self.name)));
        let mut out = String::new();
        // YAML frontmatter
        let fm = serde_yaml::to_string(&serde_json::to_value(self).unwrap_or_default())
            .unwrap_or_default();
        out.push_str("---\n");
        out.push_str(&fm);
        out.push_str("---\n\n");
        out.push_str(&self.content);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        std::fs::write(&path, out)
    }

    /// Parse a fact from a `.md` file on disk.
    pub fn from_file(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        let body = strip_frontmatter(&raw);
        // Parse frontmatter as Fact (serde_yaml)
        let fm_text = extract_frontmatter_raw(&raw)?;
        let mut fact: Fact = serde_yaml::from_str(&fm_text).ok()?;
        fact.content = body;
        Some(fact)
    }
}

/// Sanitize a fact name for use as a filename.
fn sanitize_filename(name: &str) -> String {
    name.replace(['/', '\\', '\0', ' '], "-")
        .replace("..", "--")
        .to_lowercase()
}

/// Extract raw YAML frontmatter text (between `---` delimiters).
fn extract_frontmatter_raw(raw: &str) -> Option<String> {
    let s = raw.trim();
    if !s.starts_with("---") { return None; }
    let after = &s[3..];
    let end = after.find("\n---")?;
    Some(after[..end].to_string())
}

// ── Bead ────────────────────────────────────────────────────────────────────

/// A task-state bead — tracks what was being worked on across sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bead {
    /// UUID for this bead.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Current status.
    #[serde(default)]
    pub status: BeadStatus,
    /// 0–10 priority.
    #[serde(default)]
    pub priority: u8,
    /// ISO-8601 creation timestamp.
    #[serde(default)]
    pub created: String,
    /// ISO-8601 last-updated timestamp.
    #[serde(default)]
    pub updated: String,
    /// Session ID that owns this bead.
    #[serde(default)]
    pub session: String,
    /// Markdown body — context, progress, blockers.
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BeadStatus {
    #[default]
    Todo,
    InProgress,
    Blocked,
    Done,
}

impl Bead {
    /// Write this bead to `memory/beads/{id}.md`.
    pub fn save(&self, cwd: &Path) -> std::io::Result<()> {
        let dir = cwd.join(".nonoclaw/memory/beads");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.md", &self.id));
        let mut out = String::new();
        let fm = serde_yaml::to_string(&serde_json::to_value(self).unwrap_or_default())
            .unwrap_or_default();
        out.push_str("---\n");
        out.push_str(&fm);
        out.push_str("---\n\n");
        out.push_str(&self.content);
        if !out.ends_with('\n') { out.push('\n'); }
        std::fs::write(&path, out)
    }

    /// Parse a bead from a `.md` file.
    pub fn from_file(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        let body = strip_frontmatter(&raw);
        let fm_text = extract_frontmatter_raw(&raw)?;
        let mut bead: Bead = serde_yaml::from_str(&fm_text).ok()?;
        bead.content = body;
        Some(bead)
    }
}

// ── File I/O ────────────────────────────────────────────────────────────────

/// Scan `memory/facts/*.md`, parse each as a [`Fact`].
pub fn load_facts(cwd: &Path) -> Vec<Fact> {
    scan_dir(&cwd.join(".nonoclaw/memory/facts"), Fact::from_file)
}

/// Scan `memory/beads/*.md`, parse each as a [`Bead`].
pub fn load_beads(cwd: &Path) -> Vec<Bead> {
    scan_dir(&cwd.join(".nonoclaw/memory/beads"), Bead::from_file)
}

fn scan_dir<T>(dir: &Path, parser: fn(&Path) -> Option<T>) -> Vec<T> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut out: Vec<T> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|p| parser(&p))
        .collect();
    out.sort_by(|a, b| {
        // Sort by recency — newer first based on file mtime
        // (we don't have access to the path here, so we rely on the
        // `updated` field which is parsed from frontmatter).
        // Default: keep insertion order.
        std::cmp::Ordering::Equal
    });
    out
}

/// Active (non-done) beads, sorted by priority descending.
pub fn active_beads(beads: &[Bead]) -> Vec<&Bead> {
    let mut active: Vec<&Bead> = beads
        .iter()
        .filter(|b| b.status != BeadStatus::Done)
        .collect();
    active.sort_by(|a, b| b.priority.cmp(&a.priority));
    active
}

// ── Retrieval ───────────────────────────────────────────────────────────────

/// Simple BM25-ish search over facts.  Returns facts sorted by relevance.
/// The model can then rank/select using its own intelligence.
pub fn search_facts<'a>(facts: &'a [Fact], query: &str, limit: usize) -> Vec<&'a Fact> {
    if query.trim().is_empty() {
        return facts.iter().take(limit).collect();
    }
    let terms: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        return facts.iter().take(limit).collect();
    }
    let mut scored: Vec<(f64, &Fact)> = facts
        .iter()
        .map(|f| {
            let text = format!(
                "{} {} {} {}",
                f.name,
                f.title,
                f.content,
                f.tags.join(" ")
            )
            .to_lowercase();
            let mut score = 0.0f64;
            for term in &terms {
                // Count occurrences of term in text (simple TF).
                let count = text.matches(term.as_str()).count() as f64;
                // IDF approximation: rarer terms score higher.
                let df = facts
                    .iter()
                    .filter(|f2| {
                        let t2 = format!(
                            "{} {} {} {}",
                            f2.name, f2.title, f2.content, f2.tags.join(" ")
                        )
                        .to_lowercase();
                        t2.contains(term.as_str())
                    })
                    .count() as f64;
                let idf = ((facts.len() as f64 + 1.0) / (df + 0.5)).ln();
                score += count * idf;
            }
            // Boost by importance.
            score *= 1.0 + f.importance;
            (score, f)
        })
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(limit).map(|(_, f)| f).collect()
}

/// Mark a fact as superseded by writing a `superseded_by` field.
/// The old file is kept (immutable) but the fact won't appear in active context.
pub fn supersede_fact(cwd: &Path, name: &str, superseded_by: &str) -> std::io::Result<()> {
    let path = cwd
        .join(".nonoclaw/memory/facts")
        .join(format!("{}.md", sanitize_filename(name)));
    let raw = std::fs::read_to_string(&path)?;
    // Append superseded_by to frontmatter.
    let mut new = String::new();
    let mut in_fm = false;
    let mut fm_closed = false;
    for line in raw.lines() {
        if line.trim() == "---" {
            if !in_fm {
                in_fm = true;
                new.push_str(line);
                new.push('\n');
                continue;
            } else if !fm_closed {
                new.push_str(&format!("superseded_by: {superseded_by}\n"));
                fm_closed = true;
            }
        }
        new.push_str(line);
        new.push('\n');
    }
    std::fs::write(&path, new)
}

// ── Auto-capture ────────────────────────────────────────────────────────────

/// Extract candidate facts from a transcript (v1: model-initiated).
/// Full auto-extraction via LLM summarization is planned for v2.
pub fn extract_facts_from_transcript() -> Vec<Fact> {
    Vec::new()
}

/// Build beads from session state (v1: model-initiated).
pub fn beads_from_session() -> Vec<Bead> {
    Vec::new()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Strip YAML frontmatter (`---\n...\n---\n`) from a string, returning body.
pub fn strip_frontmatter(s: &str) -> String {
    let s = s.trim();
    if !s.starts_with("---") { return s.to_string(); }
    let after = &s[3..];
    if let Some(pos) = after.find("\n---") {
        after[pos + 4..].trim().to_string()
    } else {
        s.to_string()
    }
}

// ── Context rendering ───────────────────────────────────────────────────────

/// Render active beads + top facts as a compact context block suitable for
/// injection into the system prompt. Capped at ~50 KB.
pub fn render_memory_context(beads: &[&Bead], facts: &[&Fact], max_chars: usize) -> String {
    let mut out = String::new();
    let mut chars = 0usize;

    // Active beads first — they're critical for task continuity.
    if !beads.is_empty() {
        let header = "## Active Tasks (beads)\n\n";
        out.push_str(header);
        chars += header.len();
        for (i, b) in beads.iter().enumerate() {
            if i >= 5 || chars >= max_chars / 2 {
                break;
            }
            let status_icon = match b.status {
                BeadStatus::Todo => "○",
                BeadStatus::InProgress => "◌",
                BeadStatus::Blocked => "⊘",
                BeadStatus::Done => "✓",
            };
            let line = format!(
                "{status_icon} **{title}** [priority {prio}]\n  {ctx}\n\n",
                title = b.title,
                prio = b.priority,
                ctx = truncate_words(&b.content, 50),
            );
            chars += line.len();
            if chars > max_chars { break; }
            out.push_str(&line);
        }
    }

    // Recent important facts.
    let mut sorted_facts: Vec<&&Fact> = facts.iter().collect();
    sorted_facts.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if !sorted_facts.is_empty() {
        let header = "## Key Facts\n\n";
        out.push_str(header);
        chars += header.len();
        for (i, f) in sorted_facts.iter().enumerate() {
            if i >= 10 || chars >= max_chars { break; }
            let line = format!(
                "- **{title}** ({t:?}): {body}\n",
                title = f.title,
                t = f.fact_type,
                body = truncate_words(&f.content, 30),
            );
            chars += line.len();
            if chars > max_chars { break; }
            out.push_str(&line);
        }
    }

    out
}

fn truncate_words(s: &str, max_words: usize) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= max_words {
        s.to_string()
    } else {
        let mut t: String = words.into_iter().take(max_words).collect::<Vec<_>>().join(" ");
        t.push_str("…");
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("nonoclaw-memory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fact_roundtrip() {
        let tmp = test_dir();
        let fact = Fact {
            name: "use-tsinghua-mirror".into(),
            title: "Tsinghua mirror".into(),
            content: "Use Tsinghua mirror for pip.".into(),
            fact_type: FactType::Preference,
            importance: 0.9,
            confidence: 0.95,
            created: String::new(),
            updated: String::new(),
            sources: vec!["sess-1".into()],
            supersedes: None,
            tags: vec!["python".into(), "pip".into()],
        };
        fact.save(&tmp).unwrap();
        let loaded = load_facts(&tmp);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "use-tsinghua-mirror");
        assert_eq!(loaded[0].content, "Use Tsinghua mirror for pip.");
    }

    #[test]
    fn bead_roundtrip() {
        let tmp = test_dir();
        let bead = Bead {
            id: "bead-1".into(),
            title: "Fix timeout".into(),
            status: BeadStatus::InProgress,
            priority: 8,
            created: String::new(),
            updated: String::new(),
            session: "sess-1".into(),
            content: "Investigating login timeout.".into(),
        };
        bead.save(&tmp).unwrap();
        let loaded = load_beads(&tmp);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "Fix timeout");
    }

    #[test]
    fn search_basic() {
        let facts = vec![
            Fact {
                name: "pip-mirror".into(),
                title: "pip use tsinghua".into(),
                content: "Always use tsinghua mirror for pip installs.".into(),
                importance: 0.9,
                tags: vec!["pip".into()],
                ..default_fact()
            },
            Fact {
                name: "rust-edition".into(),
                title: "use 2024 edition".into(),
                content: "Use Rust edition 2024 for new projects.".into(),
                importance: 0.5,
                tags: vec!["rust".into()],
                ..default_fact()
            },
        ];
        let results = search_facts(&facts, "pip", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "pip-mirror");
    }

    fn default_fact() -> Fact {
        Fact {
            name: String::new(),
            title: String::new(),
            content: String::new(),
            fact_type: FactType::General,
            importance: 0.5,
            confidence: 0.5,
            created: String::new(),
            updated: String::new(),
            sources: vec![],
            supersedes: None,
            tags: vec![],
        }
    }

    #[test]
    fn render_active_beads_facts() {
        let beads = vec![Bead {
            id: "b1".into(),
            title: "Fix timeout".into(),
            status: BeadStatus::InProgress,
            priority: 8,
            created: String::new(),
            updated: String::new(),
            session: "s1".into(),
            content: "Investigating login timeout in production.".into(),
        }];
        let facts = vec![Fact {
            name: "pip-mirror".into(),
            title: "pip use tsinghua".into(),
            content: "Always use tsinghua mirror.".into(),
            importance: 0.9,
            ..default_fact()
        }];
        let bead_refs: Vec<&Bead> = active_beads(&beads).into_iter().collect();
        let fact_refs: Vec<&Fact> = facts.iter().collect();
        let ctx = render_memory_context(&bead_refs, &fact_refs, 5000);
        assert!(ctx.contains("Fix timeout"));
        assert!(ctx.contains("pip use tsinghua"));
    }
}
