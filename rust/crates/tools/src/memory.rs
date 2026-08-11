//! Three-layer cross-session memory system ("Mneme").
//!
//! **Layer 1 — Facts**: immutable knowledge, one `.md` file per fact with
//!   YAML frontmatter.
//! **Layer 2 — Beads**: task state that survives sessions.
//! **Layer 3 — Transcript**: per-session JSONL.
//!
//! Facts and beads are plain markdown files under `.nonoclaw/memory/facts/` and
//! `.nonoclaw/memory/beads/`.

use std::path::Path;

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

fn default_half() -> f64 {
    0.5
}

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
    if !s.starts_with("---") {
        return None;
    }
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
        let path = dir.join(format!("{}.md", self.id));
        let mut out = String::new();
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
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|p| parser(&p))
        .collect()
}

/// Active (non-done) beads, sorted by priority descending.
pub fn active_beads(beads: &[Bead]) -> Vec<&Bead> {
    let mut active: Vec<&Bead> = beads
        .iter()
        .filter(|b| b.status != BeadStatus::Done)
        .collect();
    active.sort_by_key(|bead| std::cmp::Reverse(bead.priority));
    active
}

// ── Retrieval ───────────────────────────────────────────────────────────────

/// Hybrid search over facts: vector (trigram) similarity first, BM25 lexical
/// score as a secondary signal, importance as a final tiebreak. Returns facts
/// sorted by relevance.
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

    let vector_hits: std::collections::HashMap<String, f64> = VectorIndex::build(facts)
        .search(query, limit.saturating_mul(3))
        .into_iter()
        .collect();

    let mut scored: Vec<(f64, &Fact)> = facts
        .iter()
        .filter_map(|f| {
            let text =
                format!("{} {} {} {}", f.name, f.title, f.content, f.tags.join(" ")).to_lowercase();
            // Vector similarity (0 when the fact shares no trigrams with query).
            let vec_score = vector_hits.get(&f.name).copied().unwrap_or(0.0);
            // BM25-ish lexical score (TF × IDF approximation).
            let mut lexical = 0.0f64;
            for term in &terms {
                let count = text.matches(term.as_str()).count() as f64;
                let df = facts
                    .iter()
                    .filter(|f2| {
                        let t2 = format!(
                            "{} {} {} {}",
                            f2.name,
                            f2.title,
                            f2.content,
                            f2.tags.join(" ")
                        )
                        .to_lowercase();
                        t2.contains(term.as_str())
                    })
                    .count() as f64;
                let idf = ((facts.len() as f64 + 1.0) / (df + 0.5)).ln();
                lexical += count * idf;
            }
            let score = vec_score * 2.0 + lexical;
            if score <= 0.0 {
                return None;
            }
            // Importance boost breaks ties between near-identical relevance.
            Some((score * (1.0 + f.importance), f))
        })
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

// ── Goals (multi-step task plans, extends beads) ──────────────────────────

/// A multi-step task plan that survives sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    /// UUID for this goal.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Current status.
    #[serde(default)]
    pub status: GoalStatus,
    /// Steps with checkboxes: `[x] done`, `[ ] pending`.
    #[serde(default)]
    pub steps: Vec<String>,
    /// How to verify completion.
    #[serde(default)]
    pub verification: String,
    /// ISO-8601 creation timestamp.
    #[serde(default)]
    pub created: String,
    /// ISO-8601 last-updated timestamp.
    #[serde(default)]
    pub updated: String,
    /// Markdown body — plan, progress log.
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    #[default]
    InProgress,
    Completed,
    Blocked,
    Abandoned,
}

impl Goal {
    pub fn from_file(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        let body = strip_frontmatter(&raw);
        let fm_text = extract_frontmatter_raw(&raw)?;
        let mut goal: Goal = serde_yaml::from_str(&fm_text).ok()?;
        goal.content = body;
        Some(goal)
    }

    pub fn save(&self, cwd: &Path) -> std::io::Result<()> {
        let dir = cwd.join(".nonoclaw/memory/goals");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.md", self.id));
        let mut out = String::new();
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
}

/// Load all goals from `memory/goals/*.md`.
pub fn load_goals(cwd: &Path) -> Vec<Goal> {
    let dir = cwd.join(".nonoclaw/memory/goals");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|p| Goal::from_file(&p))
        .collect()
}

/// Active (non-completed, non-abandoned) goals.
pub fn active_goals(goals: &[Goal]) -> Vec<&Goal> {
    goals
        .iter()
        .filter(|g| g.status != GoalStatus::Completed && g.status != GoalStatus::Abandoned)
        .collect()
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

// ── Vector store (local, deterministic, dependency-free) ───────────────────
//
// Facts (and wiki pages) are embedded into fixed-dimension vectors via feature
// hashing over character trigrams, then searched by cosine similarity. This is
// a real vector store: deterministic, offline, no embedding API, no external
// crates. Persisted to `.nonoclaw/memory/.vector_index.json` and invalidated by
// a content hash so edits to a fact are picked up on the next build.

/// Embedding dimensionality. 256 dims is a good accuracy/size trade-off for
/// short markdown facts (each trigram hashes into one of 256 slots).
pub const VECTOR_DIM: usize = 256;

/// Cosine floor below which a hit is treated as hashing noise. With sign
/// hashing into 256 dims, unrelated texts land around 1/sqrt(256) ≈ 0.06;
/// genuine trigram overlap clears 0.1 comfortably (measured ~0.46 for a
/// 3-token query against a matching fact).
pub const VECTOR_NOISE_FLOOR: f64 = 0.1;

/// Stable FNV-1a 64-bit hash — deterministic across Rust versions (unlike
/// `DefaultHasher`), so persisted vectors stay valid.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Character-trigram features of a padded string (2 spaces each side) so even
/// 1-2 char queries produce at least one trigram.
fn trigram_features(text: &str) -> Vec<String> {
    let padded = format!("  {text}  ");
    let chars: Vec<char> = padded.chars().collect();
    (0..chars.len().saturating_sub(2))
        .map(|i| chars[i..i + 3].iter().collect())
        .collect()
}

/// Embed text into a fixed-dimension L2-normalized vector via hashed
/// character-trigram features (bag-of-trigrams, sign hashing for ±1 weights).
pub fn embed(text: &str) -> Vec<f64> {
    let mut vec = vec![0.0f64; VECTOR_DIM];
    let lower = text.to_lowercase();
    if lower.is_empty() {
        // No features — zero vector (cosine 0, never NaN). The padding in
        // `trigram_features` would otherwise fabricate whitespace trigrams.
        return vec;
    }
    for feature in trigram_features(&lower) {
        let hash = fnv1a(feature.as_bytes());
        let idx = (hash % VECTOR_DIM as u64) as usize;
        let sign = if hash & 1 == 0 { 1.0 } else { -1.0 };
        vec[idx] += sign;
    }
    // L2-normalize (empty text → zero vector stays zero).
    let norm: f64 = vec.iter().map(|v| v * v).sum::<f64>().sqrt();
    if norm > 0.0 {
        for v in &mut vec {
            *v /= norm;
        }
    }
    vec
}

/// Cosine similarity between two L2-normalized vectors.
pub fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// A persisted vector index over facts. Maps each fact name → embedding plus
/// the content hash used for invalidation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndex {
    pub dim: usize,
    pub facts: Vec<IndexedVector>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedVector {
    pub name: String,
    /// FNV-1a of the embedded text — a fact whose content changed re-embeds.
    pub content_hash: u64,
    pub vector: Vec<f64>,
}

impl VectorIndex {
    /// Build (or rebuild) an index from facts. The index includes all facts;
    /// `superseded_by` facts are excluded by callers via `active_facts`.
    pub fn build(facts: &[Fact]) -> Self {
        let entries = facts
            .iter()
            .map(|f| {
                let text = fact_embed_text(f);
                IndexedVector {
                    name: f.name.clone(),
                    content_hash: fnv1a(text.as_bytes()),
                    vector: embed(&text),
                }
            })
            .collect();
        VectorIndex {
            dim: VECTOR_DIM,
            facts: entries,
        }
    }

    /// Cosine-search the index, returning `(fact_name, score)` descending.
    /// Hits below [`VECTOR_NOISE_FLOOR`] are discarded as hashing noise.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(String, f64)> {
        let query_vec = embed(query);
        let mut scored: Vec<(String, f64)> = self
            .facts
            .iter()
            .map(|entry| {
                let score = cosine_similarity(&query_vec, &entry.vector);
                (entry.name.clone(), score)
            })
            .filter(|(_, score)| *score > VECTOR_NOISE_FLOOR)
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(limit).collect()
    }
}

/// Text that gets embedded for a fact: name + title + body + tags.
fn fact_embed_text(f: &Fact) -> String {
    format!("{} {} {} {}", f.name, f.title, f.content, f.tags.join(" "))
}

/// Location of the persisted vector index.
pub fn vector_index_path(cwd: &Path) -> std::path::PathBuf {
    cwd.join(".nonoclaw/memory/.vector_index.json")
}

/// Load the persisted index, rebuilding + saving when stale or missing.
///
/// Invalidation is content-hash based: the stored per-fact hash must match
/// the current text hash for every fact, otherwise the index is rebuilt.
pub fn load_or_build_vector_index(cwd: &Path, facts: &[Fact]) -> VectorIndex {
    let path = vector_index_path(cwd);
    let mut expected: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    for fact in facts {
        expected.insert(fact.name.as_str(), fnv1a(fact_embed_text(fact).as_bytes()));
    }
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(index) = serde_json::from_str::<VectorIndex>(&raw) {
            let fresh = index.dim == VECTOR_DIM
                && index.facts.len() == facts.len()
                && index.facts.iter().all(|entry| {
                    expected
                        .get(entry.name.as_str())
                        .map(|hash| *hash == entry.content_hash)
                        .unwrap_or(false)
                });
            if fresh {
                return index;
            }
        }
    }
    let index = VectorIndex::build(facts);
    if std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).is_ok() {
        if let Ok(json) = serde_json::to_string(&index) {
            let _ = std::fs::write(&path, json);
        }
    }
    index
}

/// Search facts by vector similarity, boosted by `importance` (mirrors the
/// hybrid rank used by the BM25 path: relevance first, importance breaks ties).
pub fn search_facts_vector<'a>(
    facts: &'a [Fact],
    query: &str,
    limit: usize,
) -> Vec<&'a Fact> {
    if query.trim().is_empty() {
        return facts.iter().take(limit).collect();
    }
    let index = VectorIndex::build(facts);
    let by_name: std::collections::HashMap<&str, &Fact> =
        facts.iter().map(|f| (f.name.as_str(), f)).collect();
    let mut scored: Vec<(f64, &Fact)> = index
        .search(query, limit.saturating_mul(3))
        .into_iter()
        .filter_map(|(name, score)| by_name.get(name.as_str()).map(|f| (score, *f)))
        .map(|(score, f)| (score * (1.0 + f.importance), f))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(limit).map(|(_, f)| f).collect()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Strip YAML frontmatter (`---\n...\n---\n`) from a string, returning body.
pub fn strip_frontmatter(s: &str) -> String {
    let s = s.trim();
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

// ── Wiki (LLM Wiki — Karpathy-style structured knowledge) ──────────────────

/// A structured wiki page in `.nonoclaw/wiki/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    /// Page filename without extension (used as slug for `[[links]]`).
    pub name: String,
    /// Human-readable title.
    pub title: String,
    /// Page type: concept, entity, comparison, decision, source.
    #[serde(default)]
    pub page_type: WikiType,
    /// Knowledge domain this page belongs to.
    #[serde(default)]
    pub domain: String,
    /// One-sentence summary.
    #[serde(default)]
    pub summary: String,
    /// Full markdown body.
    pub content: String,
    /// Confidence in the claims made.
    #[serde(default)]
    pub confidence: Confidence,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Source page names that back this page's claims.
    #[serde(default)]
    pub sources: Vec<String>,
    /// ISO-8601 creation timestamp.
    #[serde(default)]
    pub created: String,
    /// ISO-8601 last-updated timestamp.
    #[serde(default)]
    pub updated: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WikiType {
    #[default]
    Concept,
    Entity,
    Comparison,
    Decision,
    Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    #[default]
    Medium,
    High,
    Low,
}

impl WikiPage {
    pub fn from_file(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        let body = strip_frontmatter(&raw);
        let fm_text = extract_frontmatter_raw(&raw)?;
        let mut page: WikiPage = serde_yaml::from_str(&fm_text).ok()?;
        page.content = body;
        page.name = path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("untitled")
            .to_string();
        Some(page)
    }

    pub fn save(&self, cwd: &Path) -> std::io::Result<()> {
        let dir = cwd.join(".nonoclaw/wiki");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.md", sanitize_filename(&self.name)));
        let mut out = String::new();
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
}

/// Walk `.nonoclaw/wiki/**/*.md` recursively, parse each as a WikiPage.
pub fn load_wiki_pages(cwd: &Path) -> Vec<WikiPage> {
    let wiki_dir = cwd.join(".nonoclaw/wiki");
    walk_wiki(&wiki_dir)
}

fn walk_wiki(dir: &Path) -> Vec<WikiPage> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.append(&mut walk_wiki(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Some(page) = WikiPage::from_file(&path) {
                out.push(page);
            }
        }
    }
    out
}

/// Load `wiki/index.md` as plain text for context injection.
pub fn load_wiki_index(cwd: &Path) -> Option<String> {
    let path = cwd.join(".nonoclaw/wiki/index.md");
    let raw = std::fs::read_to_string(&path).ok()?;
    let body = strip_frontmatter(&raw);
    if body.trim().is_empty() {
        None
    } else {
        Some(body)
    }
}

/// BM25 search over wiki pages.
pub fn search_wiki<'a>(pages: &'a [WikiPage], query: &str, limit: usize) -> Vec<&'a WikiPage> {
    if query.trim().is_empty() {
        return pages.iter().take(limit).collect();
    }
    let terms: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        return pages.iter().take(limit).collect();
    }
    let mut scored: Vec<(f64, &WikiPage)> = pages
        .iter()
        .map(|p| {
            let text = format!(
                "{} {} {} {} {}",
                p.name,
                p.title,
                p.summary,
                p.content,
                p.tags.join(" ")
            )
            .to_lowercase();
            let mut score = 0.0f64;
            for term in &terms {
                let count = text.matches(term.as_str()).count() as f64;
                let df = pages
                    .iter()
                    .filter(|p2| {
                        let t2 = format!(
                            "{} {} {} {} {}",
                            p2.name,
                            p2.title,
                            p2.summary,
                            p2.content,
                            p2.tags.join(" ")
                        )
                        .to_lowercase();
                        t2.contains(term.as_str())
                    })
                    .count() as f64;
                let idf = ((pages.len() as f64 + 1.0) / (df + 0.5)).ln();
                score += count * idf;
            }
            match p.confidence {
                Confidence::High => score *= 1.2,
                Confidence::Low => score *= 0.8,
                Confidence::Medium => {}
            }
            (score, p)
        })
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(limit).map(|(_, p)| p).collect()
}

// ── Context rendering ───────────────────────────────────────────────────────

/// Render active beads + top facts as a compact context block suitable for
/// injection into the system prompt. Capped at ~50 KB. Both sections share
/// `max_chars` (beads get at most half).
pub fn render_memory_context(beads: &[&Bead], facts: &[&Fact], max_chars: usize) -> String {
    render_memory_partitioned(beads, facts, max_chars / 2, max_chars)
}

/// Render beads and facts under **independent** character caps, so one section
/// can never crowd out the other. Empty sections are skipped entirely.
pub fn render_memory_partitioned(
    beads: &[&Bead],
    facts: &[&Fact],
    beads_max_chars: usize,
    facts_max_chars: usize,
) -> String {
    let mut out = String::new();
    let mut chars = 0usize;

    // Active beads first — they're critical for task continuity.
    if !beads.is_empty() && beads_max_chars > 0 {
        let header = "## Active Tasks (beads)\n\n";
        out.push_str(header);
        chars += header.len();
        for (i, b) in beads.iter().enumerate() {
            if i >= 5 || chars >= beads_max_chars {
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
            if chars > beads_max_chars {
                break;
            }
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
    if !sorted_facts.is_empty() && facts_max_chars > 0 {
        let header = "## Key Facts\n\n";
        out.push_str(header);
        chars += header.len();
        for (i, f) in sorted_facts.iter().enumerate() {
            if i >= 10 || chars >= facts_max_chars {
                break;
            }
            let line = format!(
                "- **{title}** ({t:?}): {body}\n",
                title = f.title,
                t = f.fact_type,
                body = truncate_words(&f.content, 30),
            );
            chars += line.len();
            if chars > facts_max_chars {
                break;
            }
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
        let mut t: String = words
            .into_iter()
            .take(max_words)
            .collect::<Vec<_>>()
            .join(" ");
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    #[test]
    fn vector_embed_is_deterministic_and_normalized() {
        let a = embed("Always use tsinghua mirror for pip installs.");
        let b = embed("Always use tsinghua mirror for pip installs.");
        assert_eq!(a.len(), VECTOR_DIM);
        assert_eq!(a, b, "same text must embed identically");
        let norm: f64 = a.iter().map(|v| v * v).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-9, "L2 norm should be 1, got {norm}");
        // Empty text → zero vector (cosine = 0, never NaN).
        assert_eq!(embed("").iter().sum::<f64>(), 0.0);
    }

    #[test]
    fn vector_index_ranks_semantic_neighbours() {
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
        let index = VectorIndex::build(&facts);
        let hits = index.search("mirror installs", 2);
        assert_eq!(hits.len(), 1, "only the pip fact shares trigrams");
        assert_eq!(hits[0].0, "pip-mirror");
        assert!(hits[0].1 > 0.0);
    }

    #[test]
    fn vector_search_handles_misspelled_query() {
        // Trigram overlap tolerates a single typo better than exact BM25 match.
        let facts = vec![
            Fact {
                name: "deployment".into(),
                title: "deploy pipeline".into(),
                content: "The deployment pipeline runs cargo test on every push.".into(),
                importance: 0.6,
                tags: vec!["ci".into()],
                ..default_fact()
            },
            Fact {
                name: "billing".into(),
                title: "api billing".into(),
                content: "Billing counters track tokens per provider.".into(),
                importance: 0.4,
                tags: vec!["cost".into()],
                ..default_fact()
            },
        ];
        let results = search_facts_vector(&facts, "deploment pipeline", 2);
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "deployment");
    }

    #[test]
    fn vector_index_roundtrip_via_disk() {
        let tmp = test_dir();
        let fact = Fact {
            name: "mirror".into(),
            title: "pip mirror".into(),
            content: "Use the tsinghua mirror for pip.".into(),
            importance: 0.8,
            tags: vec![],
            ..default_fact()
        };
        fact.save(&tmp).unwrap();
        let facts = load_facts(&tmp);
        let index = load_or_build_vector_index(&tmp, &facts);
        assert_eq!(index.facts.len(), 1);
        let path = vector_index_path(&tmp);
        assert!(path.exists(), "index must be persisted to disk");
        // Second load reuses the on-disk index (no rebuild drift).
        let reloaded = load_or_build_vector_index(&tmp, &facts);
        assert_eq!(reloaded.facts[0].vector, index.facts[0].vector);
        // Query hits the loaded fact.
        assert_eq!(reloaded.search("pip install", 1)[0].0, "mirror");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn vector_index_rebuilds_when_fact_content_changes() {
        let tmp = test_dir();
        let mut fact = Fact {
            name: "mirror".into(),
            title: "pip mirror".into(),
            content: "Use the tsinghua mirror for pip.".into(),
            importance: 0.8,
            tags: vec![],
            ..default_fact()
        };
        fact.save(&tmp).unwrap();
        let mut facts = load_facts(&tmp);
        let index = load_or_build_vector_index(&tmp, &facts);
        assert_eq!(index.facts.len(), 1);
        // Edit the fact content → the persisted index is now stale.
        fact.content = "Prefer the aliyun mirror for all package installs.".into();
        fact.save(&tmp).unwrap();
        facts = load_facts(&tmp);
        let current_hash = fnv1a(fact_embed_text(&facts[0]).as_bytes());
        assert_ne!(
            index.facts[0].content_hash, current_hash,
            "precondition: stale index must disagree with current content"
        );
        let rebuilt = load_or_build_vector_index(&tmp, &facts);
        assert_eq!(
            rebuilt.facts[0].content_hash, current_hash,
            "stale index should have been rebuilt to match current content"
        );
        // The rebuilt index ranks the new content first.
        let hits = rebuilt.search("aliyun installs", 1);
        assert!(!hits.is_empty(), "new content must be searchable");
        let _ = std::fs::remove_dir_all(&tmp);
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
        let facts = [Fact {
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
