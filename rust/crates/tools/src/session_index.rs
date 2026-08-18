//! Vector index over full session transcripts (JSONL) for cross-session recall.
//!
//! Layers 1-2 of Mneme (facts/beads) are indexed by [`crate::memory`]. This
//! module covers **Layer 3 — transcripts**: every persisted session file under
//! `~/.nonoclaw/projects/<sanitized-cwd>/sessions/*.jsonl` is parsed, its
//! user/assistant text (plus thinking summaries) is embedded with the same
//! dependency-free trigram hasher, and the vectors are persisted compactly
//! (i8-quantized + base64) to `.nonoclaw/memory/.session_index.json`.
//!
//! Incremental rebuild: each session file's byte size + mtime is remembered;
//! unchanged files keep their embeddings, changed/new files re-embed, removed
//! files drop out. Build is safe to run from multiple processes — last writer
//! wins, and staleness is detected by the same file fingerprints.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::memory::{cosine_similarity, embed, VECTOR_DIM, VECTOR_NOISE_FLOOR};

/// Max characters of a message embedded per chunk. Sessions contain long
/// turns; one vector per message keeps recall fine-grained without the index
/// exploding (7.2k messages → ~7k entries × 256 dims i8 ≈ 1.8 MB base64).
const MAX_CHUNK_CHARS: usize = 2000;

/// Where the session index lives (per-project, next to the facts index).
pub fn session_index_path(cwd: &Path) -> PathBuf {
    cwd.join(".nonoclaw/memory/.session_index.json")
}

/// One embedded chunk of a session. `text` is kept for hit display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionChunk {
    /// Session file stem (UUID).
    pub session_id: String,
    /// 1-based message index within the session (display only).
    pub index: usize,
    /// `user` | `assistant` | `summary`.
    pub role: String,
    /// Embedded text (already truncated to MAX_CHUNK_CHARS).
    pub text: String,
}

/// Compact wire form of a vector: i8 quantization of [-1,1] then base64.
fn encode_vector(vec: &[f64]) -> String {
    let bytes: Vec<u8> = vec
        .iter()
        .map(|v| (v.clamp(-1.0, 1.0) * 127.0).round() as i8 as u8)
        .collect();
    base64_encode(&bytes)
}

fn decode_vector(encoded: &str) -> Vec<f64> {
    let bytes = match base64_decode(encoded) {
        Some(b) => b,
        None => return vec![0.0; VECTOR_DIM],
    };
    let mut out = Vec::with_capacity(bytes.len());
    for b in bytes {
        out.push(b as i8 as f64 / 127.0);
    }
    out
}

/// Minimal base64 (standard alphabet, padded) without a new dependency.
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

fn base64_decode(data: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for c in data.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let val = TABLE.iter().position(|&t| t == c)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// File fingerprint used for incremental rebuilds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStamp {
    pub len: u64,
    pub mtime_ms: u64,
}

/// The persisted session vector index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIndex {
    pub dim: usize,
    /// session file stem → fingerprint at embed time.
    pub stamps: HashMap<String, FileStamp>,
    /// base64(i8-quantized) embedding per chunk.
    pub vectors: Vec<String>,
    /// chunk metadata, parallel to `vectors`.
    pub chunks: Vec<SessionChunk>,
}

impl Default for SessionIndex {
    fn default() -> Self {
        Self {
            dim: VECTOR_DIM,
            stamps: HashMap::new(),
            vectors: Vec::new(),
            chunks: Vec::new(),
        }
    }
}

fn stamp_of(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some(FileStamp {
        len: meta.len(),
        mtime_ms,
    })
}

/// Extract embeddable text chunks from one JSONL session file.
///
/// Chunks: user text, assistant text blocks, assistant thinking blocks
/// (truncated), and `summary` entries. Tool use/result blocks are skipped —
/// they carry no prose for recall.
pub fn parse_session_chunks(path: &Path, session_id: &str) -> Vec<SessionChunk> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut index = 0usize;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        index += 1;
        match value.get("kind").and_then(|k| k.as_str()) {
            Some("summary") => {
                if let Some(text) = value.get("text").and_then(|t| t.as_str()) {
                    push_chunk(&mut out, session_id, index, "summary", text);
                }
            }
            Some("message") => {
                let role = value
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("user")
                    .to_string();
                match value.get("content") {
                    Some(serde_json::Value::String(text)) => {
                        push_chunk(&mut out, session_id, index, &role, text);
                    }
                    Some(serde_json::Value::Array(blocks)) => {
                        for block in blocks {
                            let block_type = block.get("type").and_then(|t| t.as_str());
                            match block_type {
                                Some("text") => {
                                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                        push_chunk(&mut out, session_id, index, &role, t);
                                    }
                                }
                                Some("thinking") if role == "assistant" => {
                                    // Thinking text can be huge; keep the first slice only.
                                    if let Some(t) = block.get("thinking").and_then(|t| t.as_str())
                                    {
                                        let head: String =
                                            t.chars().take(400).collect();
                                        if !head.trim().is_empty() {
                                            push_chunk(
                                                &mut out,
                                                session_id, index, "thinking", &head,
                                            );
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

fn push_chunk(out: &mut Vec<SessionChunk>, session_id: &str, index: usize, role: &str, text: &str) {
    let truncated: String = text.chars().take(MAX_CHUNK_CHARS).collect();
    if truncated.trim().is_empty() {
        return;
    }
    out.push(SessionChunk {
        session_id: session_id.to_string(),
        index,
        role: role.to_string(),
        text: truncated,
    });
}

/// Load the persisted index if present.
pub fn load_index(cwd: &Path) -> SessionIndex {
    match std::fs::read_to_string(session_index_path(cwd)) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => SessionIndex::default(),
    }
}

/// Build (or incrementally refresh) the index over the sessions directory.
///
/// `sessions_dir` is typically `~/.nonoclaw/projects/<sanitized-cwd>/sessions`.
/// Returns the fresh index; persistence is best-effort.
pub fn build_index(cwd: &Path, sessions_dir: &Path) -> SessionIndex {
    let mut index = load_index(cwd);
    if index.dim != VECTOR_DIM {
        index = SessionIndex::default();
    }
    let mut current: HashMap<String, FileStamp> = HashMap::new();
    let mut keep: Vec<bool> = index.chunks.iter().map(|_| false).collect();
    if let Ok(read) = std::fs::read_dir(sessions_dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(stamp) = stamp_of(&path) else { continue };
            current.insert(stem.to_string(), stamp.clone());
            let unchanged = index
                .stamps
                .get(stem)
                .map(|old| old.len == stamp.len && old.mtime_ms == stamp.mtime_ms)
                .unwrap_or(false);
            if unchanged {
                for (i, chunk) in index.chunks.iter().enumerate() {
                    if chunk.session_id == stem {
                        keep[i] = true;
                    }
                }
                continue;
            }
            // Re-embed this file: drop old chunks for it, append fresh ones.
            for (i, chunk) in index.chunks.iter().enumerate() {
                if chunk.session_id == stem {
                    keep[i] = false;
                }
            }
            for chunk in parse_session_chunks(&path, stem) {
                index.vectors.push(encode_vector(&embed(&chunk.text)));
                index.chunks.push(chunk);
            }
        }
    }
    // Compact: drop chunks whose session file vanished. Chunks appended during
    // this run (beyond keep's original length) are current by construction.
    keep.resize(index.chunks.len(), true);
    if keep.iter().any(|k| !*k) {
        let mut vectors = Vec::with_capacity(index.vectors.len());
        let mut chunks = Vec::with_capacity(index.chunks.len());
        for (i, k) in keep.into_iter().enumerate() {
            if k {
                vectors.push(index.vectors[i].clone());
                chunks.push(index.chunks[i].clone());
            }
        }
        index.vectors = vectors;
        index.chunks = chunks;
    }
    index.stamps = current;
    index.dim = VECTOR_DIM;
    if let Err(e) = std::fs::create_dir_all(
        session_index_path(cwd)
            .parent()
            .unwrap_or(Path::new(".")),
    ) {
        tracing::warn!(error = %e, "cannot create memory dir for session index");
        return index;
    }
    match serde_json::to_string(&index) {
        Ok(json) => {
            if let Err(e) = std::fs::write(session_index_path(cwd), json) {
                tracing::warn!(error = %e, "cannot persist session index");
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot serialize session index"),
    }
    index
}

/// A search hit over the session index.
#[derive(Debug, Clone)]
pub struct SessionHit {
    pub chunk: SessionChunk,
    pub score: f64,
}

/// Cosine-search the session index. Noise-floor filtered, best first.
pub fn search(index: &SessionIndex, query: &str, limit: usize) -> Vec<SessionHit> {
    if query.trim().is_empty() || index.chunks.is_empty() {
        return Vec::new();
    }
    let query_vec = embed(query);
    let mut scored: Vec<SessionHit> = index
        .vectors
        .iter()
        .zip(index.chunks.iter())
        .map(|(encoded, chunk)| SessionHit {
            chunk: chunk.clone(),
            score: cosine_similarity(&query_vec, &decode_vector(encoded)),
        })
        .filter(|hit| hit.score > VECTOR_NOISE_FLOOR)
        .collect();
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &Path, id: &str, lines: &[String]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn base64_roundtrip() {
        for len in [0usize, 1, 2, 3, 64, 255, 256] {
            let data: Vec<u8> = (0..len as u8).map(|i| i.wrapping_mul(37).wrapping_add(len as u8)).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data, "len={len}");
        }
    }

    #[test]
    fn quantized_vector_roundtrip_keeps_similarity_sign() {
        let a = embed("deployment pipeline failure");
        let b = embed("deployment pipeline fails");
        let c = embed("completely unrelated text about cooking recipes");
        let aq = decode_vector(&encode_vector(&a));
        let bq = decode_vector(&encode_vector(&b));
        let cq = decode_vector(&encode_vector(&c));
        assert!(cosine_similarity(&a, &b) > cosine_similarity(&a, &c));
        assert!(cosine_similarity(&aq, &bq) > cosine_similarity(&aq, &cq));
        assert!(cosine_similarity(&aq, &bq) > 0.3);
    }

    #[test]
    fn parse_chunks_extracts_prose_and_skips_tools() {
        let dir = std::env::temp_dir().join(format!("sessidx-{}", uuid::Uuid::new_v4()));
        let lines = vec![
            r#"{"kind":"session","id":"s1","cwd":"/p","model":"m","started":"2026"}"#.into(),
            r#"{"kind":"message","role":"user","content":"fix the login bug in auth.rs"}"#.into(),
            r#"{"kind":"message","role":"assistant","content":[{"type":"thinking","thinking":"long reasoning here"},{"type":"text","text":"I will fix it."},{"type":"tool_use","id":"t1","name":"Read","input":{}}]}"#.into(),
            r#"{"kind":"message","role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file body"}]}"#.into(),
        ];
        write_session(&dir, "s1", &lines);
        let chunks = parse_session_chunks(&dir.join("s1.jsonl"), "s1");
        let roles: Vec<&str> = chunks.iter().map(|c| c.role.as_str()).collect();
        assert!(roles.contains(&"user"));
        assert!(roles.contains(&"assistant"));
        assert!(roles.contains(&"thinking"));
        assert!(!roles.contains(&"tool"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_is_incremental_and_search_finds_recall() {
        let cwd = std::env::temp_dir().join(format!("sessidx2-{}", uuid::Uuid::new_v4()));
        let sessions = cwd.join("sessions");
        std::fs::create_dir_all(&cwd.join(".nonoclaw/memory")).unwrap();
        write_session(
            &sessions,
            "aaa",
            &[
                r#"{"kind":"message","role":"user","content":"the kafka consumer keeps rebalancing in prod"}"#.into(),
            ],
        );
        let index = build_index(&cwd, &sessions);
        assert_eq!(index.chunks.len(), 1);
        let hits = search(&index, "kafka rebalancing", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk.session_id, "aaa");
        assert!(hits[0].score > 0.3);

        // Unchanged file → index stays but is reloaded cleanly.
        let again = build_index(&cwd, &sessions);
        assert_eq!(again.chunks.len(), 1);

        // New session + removed old file.
        std::fs::remove_file(sessions.join("aaa.jsonl")).unwrap();
        write_session(
            &sessions,
            "bbb",
            &[r#"{"kind":"message","role":"user","content":"rust borrow checker error"}"#.into()],
        );
        let third = build_index(&cwd, &sessions);
        assert_eq!(third.chunks.len(), 1);
        assert_eq!(third.chunks[0].session_id, "bbb");
        std::fs::remove_dir_all(&cwd).ok();
    }
}
