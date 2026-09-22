//! Canonical session persistence and resume service.
//!
//! Every persisted session is owned by exactly one writer actor in this
//! process. All transcript and metadata mutations are serialized through that
//! actor so the in-memory revision and JSONL order advance together.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};

use nonoclaw_core::Message;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Tag stamped on background AutoDream consolidation sessions. Lets the UI and
/// `most_recent_session` distinguish machine-generated transcripts from the
/// user's working sessions.
pub const DREAM_SESSION_TAG: &str = "dream";

/// Tag for bench-smoke harness sessions (`nonoclaw --tag bench-smoke`, driven
/// by `bench/terminal-bench/run_local_smoke.py` after every dream). Same
/// exclusion semantics as dream sessions: auto-resume must not land on them.
pub const BENCH_SMOKE_SESSION_TAG: &str = "bench-smoke";

/// One JSONL line in a session file. The wire representation is retained for
/// compatibility with all existing session files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEntry {
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
    /// Versioned, redacted replay facts for one completed root run. Version 1
    /// is the legacy timing-only batch; version 2 keeps the complete low-
    /// frequency root stream plus scoped child lifecycle/tool facts.
    Trace {
        #[serde(default = "legacy_trace_schema_version")]
        schema_version: u16,
        run_id: String,
        events: Vec<nonoclaw_core::run_event::EventEnvelope>,
    },
    /// Outcome metadata for one completed run — the trajectory-level reward
    /// label (Level-1 RL data): terminal status, heuristic reward score,
    /// turn count, and a human-readable finish detail. Appended exactly once
    /// per run, right after the terminal event.
    RunOutcome {
        run_id: String,
        /// "done" | "cancelled" | "error"
        status: String,
        /// Heuristic reward: done=1.0, cancelled=-0.3, error=-1.0,
        /// with reductions for max_turns/budget/context-limit exhaustion.
        reward: f64,
        turns: u32,
        /// Short finish detail (completed message / cancel reason / error kind).
        detail: String,
        /// Wall-clock unix seconds when this outcome was appended. Used by
        /// the dream ledger to distinguish new runs appended to an
        /// already-analyzed session. `default` keeps old JSONL parseable.
        #[serde(default)]
        ts: u64,
    },
    /// Running total of real API token usage (accumulated across all
    /// completed runs). Used to restore the frontend right-rail in/out
    /// display across server restarts.
    CumulativeUsage {
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
    },
}

pub use nonoclaw_core::{SessionRepair, SessionRepairKind};

/// A revisioned view of one session. Revisions increase exactly once per
/// successful mutation command, regardless of how many JSONL lines it writes.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub revision: u64,
    pub started: Option<String>,
    pub summary: String,
    pub messages: Vec<Message>,
    pub title: Option<String>,
    pub tag: Option<String>,
    pub mode: Option<String>,
    pub repairs: Vec<SessionRepair>,
    /// Cumulative real API token usage from all completed runs, persisted
    /// across server restarts. `None` if no runs have completed yet.
    pub cumulative_usage: Option<CumulativeUsageWire>,
    /// Persisted timing traces (one batch per completed run) for accurate
    /// ledger replay. Empty for legacy sessions recorded before traces
    /// existed; the frontend falls back to message-`ts` inference.
    pub traces: Vec<PersistedTraceWire>,
}

/// Wire-friendly representation of cumulative token usage, used in both
/// SessionSnapshot and the WS `messages_loaded` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CumulativeUsageWire {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// Current persisted replay schema. Older trace lines omit the field and are
/// interpreted as version 1 without rewriting their original JSON.
pub const TRACE_SCHEMA_VERSION: u16 = 2;

fn legacy_trace_schema_version() -> u16 {
    1
}

/// One run's versioned replay facts, as sent in `messages_loaded`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTraceWire {
    #[serde(default = "legacy_trace_schema_version")]
    pub schema_version: u16,
    pub run_id: String,
    pub events: Vec<nonoclaw_core::run_event::EventEnvelope>,
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
    /// Number of completed runs with a persisted reward label (RunOutcome).
    pub run_outcomes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("session writer is closed")]
    Closed,
    #[error("session revision conflict: expected {expected}, current {current}")]
    RevisionConflict { expected: u64, current: u64 },
}

pub type SessionResult<T> = std::result::Result<T, SessionError>;

type Reply<T> = oneshot::Sender<SessionResult<T>>;

enum SessionCommand {
    AppendMessage(Message, Reply<u64>),
    ReplaceAfterCompact {
        messages: Vec<Message>,
        expected_revision: u64,
        reply: Reply<u64>,
    },
    Clear(Reply<u64>),
    AppendMetadata(SessionEntry, Reply<u64>),
    Snapshot(Reply<SessionSnapshot>),
    HistoryPage { before: usize, limit: usize, reply: Reply<SessionHistoryPage> },
}

/// One `load_older` page: messages strictly before `before`, ascending,
/// with the session revision for ordering checks.
#[derive(Debug, Clone)]
pub struct SessionHistoryPage {
    pub revision: u64,
    pub messages: Vec<Message>,
    /// Number of messages older than this page (0 = start reached).
    pub remaining: usize,
}

struct SessionInner {
    id: String,
    path: PathBuf,
    tx: mpsc::Sender<SessionCommand>,
}

/// Cloneable command handle for one canonical session writer.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

impl Session {
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub async fn snapshot(&self) -> SessionResult<SessionSnapshot> {
        self.request(SessionCommand::Snapshot).await
    }

    /// Read one page of older messages (strictly before `before`, ascending)
    /// without materializing a full snapshot. Used by UI history paging.
    pub async fn history_page(&self, before: usize, limit: usize) -> SessionResult<SessionHistoryPage> {
        self.request(|reply| SessionCommand::HistoryPage { before, limit, reply })
            .await
    }

    pub async fn append(&self, message: Message) -> SessionResult<u64> {
        self.request(|reply| SessionCommand::AppendMessage(message, reply))
            .await
    }

    pub async fn replace_after_compact(
        &self,
        messages: Vec<Message>,
        expected_revision: u64,
    ) -> SessionResult<u64> {
        self.request(|reply| SessionCommand::ReplaceAfterCompact {
            messages,
            expected_revision,
            reply,
        })
        .await
    }

    pub async fn clear(&self) -> SessionResult<u64> {
        self.request(SessionCommand::Clear).await
    }

    pub async fn write_custom_title(&self, title: impl Into<String>) -> SessionResult<u64> {
        self.append_metadata(SessionEntry::CustomTitle {
            title: title.into(),
        })
        .await
    }

    pub async fn write_ai_title(&self, title: impl Into<String>) -> SessionResult<u64> {
        self.append_metadata(SessionEntry::AiTitle {
            title: title.into(),
        })
        .await
    }

    pub async fn write_last_prompt(&self, prompt: impl Into<String>) -> SessionResult<u64> {
        self.append_metadata(SessionEntry::LastPrompt {
            prompt: prompt.into(),
        })
        .await
    }

    pub async fn write_tag(&self, tag: impl Into<String>) -> SessionResult<u64> {
        self.append_metadata(SessionEntry::Tag { tag: tag.into() })
            .await
    }

    pub async fn write_mode(&self, mode: impl Into<String>) -> SessionResult<u64> {
        self.append_metadata(SessionEntry::Mode { mode: mode.into() })
            .await
    }

    /// Append a Level-1 RL reward label for one completed run.
    pub async fn write_run_outcome(
        &self,
        run_id: &str,
        status: &str,
        reward: f64,
        turns: u32,
        detail: &str,
    ) -> SessionResult<u64> {
        self.append_metadata(SessionEntry::RunOutcome {
            run_id: run_id.to_string(),
            status: status.to_string(),
            reward,
            turns,
            detail: detail.to_string(),
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
        .await
    }

    /// Persist one completed root run's complete low-frequency replay facts.
    /// Content deltas are filtered by `TraceCollector`; durable boundaries are
    /// intentionally not count-truncated because replay must remain exact.
    pub async fn write_trace(
        &self,
        run_id: &str,
        events: Vec<nonoclaw_core::run_event::EventEnvelope>,
    ) -> SessionResult<u64> {
        self.append_metadata(SessionEntry::Trace {
            schema_version: TRACE_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            events,
        })
        .await
    }

    pub async fn write_usage(&self, usage: &CumulativeUsageWire) -> SessionResult<u64> {
        self.append_metadata(SessionEntry::CumulativeUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
        })
        .await
    }

    pub async fn write_summary(&self, text: impl Into<String>) -> SessionResult<u64> {
        self.append_metadata(SessionEntry::Summary { text: text.into() })
            .await
    }

    async fn append_metadata(&self, entry: SessionEntry) -> SessionResult<u64> {
        self.request(|reply| SessionCommand::AppendMetadata(entry, reply))
            .await
    }

    async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> SessionCommand) -> SessionResult<T> {
        let (reply, receive) = oneshot::channel();
        self.inner
            .tx
            .send(make(reply))
            .map_err(|_| SessionError::Closed)?;
        receive.await.map_err(|_| SessionError::Closed)?
    }
}

/// Canonical owner of session discovery, loading, and actor creation.
///
/// `list_sessions` fingerprints each JSONL by (len, mtime) and reuses the
/// previously parsed `SessionInfo` when the file is untouched, so listing a
/// large project stays cheap. All clones share one process-wide cache cell;
/// writing is always persisted to the JSONL first, the cache may only lag.
#[derive(Debug, Clone, Default)]
pub struct SessionService {
    list_cache: Arc<CacheCell<Mutex<HashMap<PathBuf, ListCache>>>>,
}

/// Per-sessions-dir list cache: file fingerprint → parsed metadata.
type ListCache = HashMap<String, CachedSessionInfo>;

#[derive(Debug, Clone)]
struct CachedSessionInfo {
    len: u64,
    mtime_ms: u64,
    info: SessionInfo,
}

fn writer_registry() -> &'static Mutex<HashMap<PathBuf, Weak<SessionInner>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<SessionInner>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Interior-mutable Default (a `Default` impl that produces an unset cell).
struct CacheCell<T: Default> {
    value: OnceLock<T>,
}

impl<T: Default> std::fmt::Debug for CacheCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheCell")
    }
}

impl<T: Default> Default for CacheCell<T> {
    fn default() -> Self {
        Self { value: OnceLock::new() }
    }
}

impl<T: Default> CacheCell<T> {
    fn get(&self) -> &T {
        self.value.get_or_init(T::default)
    }
}

impl SessionService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a fresh lazily-persisted session. The header is written together
    /// with its first command so abandoned sessions leave no empty files.
    pub fn create(
        &self,
        cwd: &Path,
        id: impl Into<String>,
        model: impl Into<String>,
    ) -> SessionResult<Session> {
        let id = id.into();
        let path = session_path(cwd, &id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot determine session path (set HOME or NONOCLAW_HOME)",
            )
        })?;
        let header = SessionEntry::Session {
            id: id.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            model: model.into(),
            started: chrono::Local::now().to_rfc3339(),
        };
        self.open_actor(path, id, Some(header), false)
    }

    /// Resume an existing session and surface any recoverable legacy damage in
    /// its snapshot. Malformed lines are skipped; valid unknown lines survive
    /// future compact rewrites.
    pub fn resume(&self, cwd: &Path, id: &str) -> SessionResult<Session> {
        let path = session_path(cwd, id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot determine session path (set HOME or NONOCLAW_HOME)",
            )
        })?;
        let fallback = SessionEntry::Session {
            id: id.to_string(),
            cwd: cwd.to_string_lossy().to_string(),
            model: String::new(),
            started: chrono::Local::now().to_rfc3339(),
        };
        self.open_actor(path, id.to_string(), Some(fallback), true)
    }

    /// Open an explicit path. This supports embedders and focused tests while
    /// still going through the process-wide single-writer registry.
    pub fn open_path(
        &self,
        path: PathBuf,
        id: impl Into<String>,
        cwd: &Path,
        model: impl Into<String>,
    ) -> SessionResult<Session> {
        let id = id.into();
        let header = SessionEntry::Session {
            id: id.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            model: model.into(),
            started: chrono::Local::now().to_rfc3339(),
        };
        let must_exist = path.exists();
        self.open_actor(path, id, Some(header), must_exist)
    }

    fn open_actor(
        &self,
        path: PathBuf,
        id: String,
        fallback_header: Option<SessionEntry>,
        must_exist: bool,
    ) -> SessionResult<Session> {
        let path = absolute_path(path)?;
        let mut registry = writer_registry().lock().unwrap();
        if let Some(existing) = registry.get(&path).and_then(Weak::upgrade) {
            return Ok(Session { inner: existing });
        }
        if must_exist && !path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("session {} not found", path.display()),
            )
            .into());
        }

        let state = if path.exists() {
            SessionState::load(&path, fallback_header)?
        } else {
            SessionState::fresh(fallback_header.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing session header")
            })?)
        };
        let (tx, rx) = mpsc::channel();
        let inner = Arc::new(SessionInner {
            id,
            path: path.clone(),
            tx,
        });
        registry.insert(path.clone(), Arc::downgrade(&inner));
        std::thread::Builder::new()
            .name(format!("session-writer-{}", inner.id))
            .spawn(move || writer_loop(path, state, rx))?;
        Ok(Session { inner })
    }

    pub fn list_sessions(&self, cwd: &Path) -> std::io::Result<Vec<SessionInfo>> {
        let Some(dir) = project_dir(cwd) else {
            return Ok(Vec::new());
        };
        let sessions_dir = dir.join("sessions");
        let mut out = Vec::new();
        let read = match std::fs::read_dir(&sessions_dir) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut cache = self.list_cache.get().lock().unwrap();
        let cache = cache.entry(sessions_dir.clone()).or_default();
        let mut seen = Vec::new();
        for entry in read {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let metadata = entry.metadata()?;
            let mtime = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
            let len = metadata.len();
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("")
                .to_string();
            seen.push(id.clone());
            let mtime_ms = u64::try_from(
                mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis())
                    .unwrap_or(0),
            )
            .unwrap_or(0);
            if let Some(cached) = cache.get(&id) {
                if cached.len == len && cached.mtime_ms == mtime_ms {
                    out.push(cached.info.clone());
                    continue;
                }
            }
            let state = match SessionState::load(&path, None) {
                Ok(state) => state,
                Err(_) => continue,
            };
            let summary = if state.summary.is_empty() {
                state
                    .messages
                    .iter()
                    .find_map(|message| match &message.content {
                        nonoclaw_core::MessageContent::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            } else {
                state.summary.clone()
            };
            // The UI shows a single-line ellipsis preview; sessions restored
            // from compaction summaries otherwise ship their whole first text
            // message (often multi-KB) and the session_list frame balloons
            // past WebSocket size limits on large projects.
            let summary = truncate_chars(&summary, LIST_SUMMARY_MAX_CHARS);
            let title = state.title();
            let run_outcomes = state
                .preserved
                .iter()
                .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("run_outcome"))
                .count();
            let info = SessionInfo {
                id: id.clone(),
                started: state.started,
                message_count: state.messages.len(),
                summary,
                title,
                tag: state.tag,
                mtime,
                run_outcomes,
            };
            cache.insert(id, CachedSessionInfo { len, mtime_ms, info: info.clone() });
            out.push(info);
        }
        cache.retain(|id, _| seen.contains(id));
        out.sort_by_key(|session| std::cmp::Reverse(session.mtime));
        Ok(out)
    }

    /// The most recent session to auto-resume, skipping background-generated
    /// ones (AutoDream consolidation, bench-smoke harness runs) so the Web UI
    /// lands on the user's last working session, not a machine transcript.
    pub fn most_recent_session(&self, cwd: &Path) -> std::io::Result<Option<String>> {
        Ok(self
            .list_sessions(cwd)?
            .into_iter()
            .find(|info| !matches!(
                info.tag.as_deref(),
                Some(DREAM_SESSION_TAG) | Some(BENCH_SMOKE_SESSION_TAG)
            ))
            .map(|info| info.id))
    }
}

/// Heuristic trajectory reward for one completed run (Level-1 RL label).
///
/// Signals: terminal status is authoritative (done > cancelled > error);
/// exhaustion finishes (max-turns / budget / context-limit) reduce a "done"
/// run because the agent stalled instead of converging.
pub fn run_reward(status: &str, finish_detail: &str) -> f64 {
    run_reward_labeled(status, finish_detail, &RewardSignals::default())
}

/// Objective (label-free) signals observed during a run — no LLM judge, only
/// mechanically verifiable facts from the event stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct RewardSignals {
    /// Tools finishing `Succeeded`/`Repaired`.
    pub tools_ok: u32,
    /// Tools finishing `Failed`.
    pub tools_failed: u32,
}

impl RewardSignals {
    fn tool_error_rate(&self) -> Option<f64> {
        let total = self.tools_ok + self.tools_failed;
        (total >= 3).then(|| self.tools_failed as f64 / total as f64)
    }
}

/// Reward with label-free adjustments layered on the terminal-status heuristic:
/// - **verification evidence bonus** (+0.15, capped at 1.0): the transcript's
///   finish detail contains a passing test/build/lint run — the strongest
///   objective "task actually works" signal available without ground truth.
/// - **high tool error rate penalty** (−0.3 at ≥60%, −0.15 at ≥30%): a run
///   that mostly fights its tools rarely reflects good orchestration, even
///   when it technically completes.
pub fn run_reward_labeled(status: &str, finish_detail: &str, signals: &RewardSignals) -> f64 {
    let base = run_reward_base(status, finish_detail);
    if status != "done" {
        return base;
    }
    let mut reward = base;
    if verification_evidence(finish_detail) {
        reward += 0.15;
    }
    if let Some(rate) = signals.tool_error_rate() {
        if rate >= 0.6 {
            reward -= 0.3;
        } else if rate >= 0.3 {
            reward -= 0.15;
        }
    }
    reward.clamp(-1.0, 1.0)
}

fn run_reward_base(status: &str, finish_detail: &str) -> f64 {
    const DONE: f64 = 1.0;
    const CANCELLED: f64 = -0.3;
    const ERROR: f64 = -1.0;
    let d = finish_detail.to_lowercase();
    let exhaustion_penalty = if d.contains("mid-thinking") {
        // max_tokens hit while the model was still thinking — the turn
        // produced no usable output at all, yet the run still finishes as
        // "done". Same weight as max-turns exhaustion. Checked before the
        // generic branches because the engine's detail text also mentions
        // the output budget.
        0.4
    } else if d.contains("max turns") || d.contains("max_turns") {
        0.4
    } else if d.contains("budget") {
        0.3
    } else if d.contains("context limit") || d.contains("context_limit") {
        0.2
    } else {
        0.0
    };
    match status {
        "done" => DONE - exhaustion_penalty,
        "cancelled" => CANCELLED - exhaustion_penalty * 0.5,
        _ => ERROR,
    }
}

/// Does the finish detail contain evidence of a *passing* verification run?
/// Intentionally narrow patterns to avoid false positives (e.g. "0 tests
/// passed" on an empty suite is not evidence).
fn verification_evidence(detail: &str) -> bool {
    let d = detail.to_lowercase();
    // "N passed" with N ≥ 1 (cargo/pytest/jest style), "all tests passed",
    // "tests: ok", "npm test" followed by pass markers.
    for pat in ["test result: ok", "all tests passed", "tests passed"] {
        if d.contains(pat) {
            // Guard against "0 tests passed".
            if pat == "tests passed" && d.contains("0 tests passed") {
                continue;
            }
            return true;
        }
    }
    if let Some(idx) = d.find(" passed") {
        // Look back for a digit prefix: "12 passed", "3 passed".
        let prefix = &d[..idx];
        if prefix.chars().rev().find(|c| !c.is_whitespace()).is_some_and(|c| c.is_ascii_digit())
            && !prefix.ends_with('0')
        {
            return true;
        }
    }
    false
}

struct SessionState {
    revision: u64,
    header: SessionEntry,
    preserved: Vec<serde_json::Value>,
    started: Option<String>,
    summary: String,
    messages: Vec<Message>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    tag: Option<String>,
    mode: Option<String>,
    cumulative_usage: Option<CumulativeUsageWire>,
    /// Per-root-run versioned replay batches, keyed by run_id and retained in
    /// JSONL insertion order (a duplicate run replaces the prior snapshot).
    traces: Vec<PersistedTraceWire>,
    repairs: Vec<SessionRepair>,
    needs_rewrite: bool,
}

impl SessionState {
    fn fresh(header: SessionEntry) -> Self {
        let started = match &header {
            SessionEntry::Session { started, .. } => Some(started.clone()),
            _ => None,
        };
        Self {
            revision: 0,
            header,
            preserved: Vec::new(),
            started,
            summary: String::new(),
            messages: Vec::new(),
            custom_title: None,
            ai_title: None,
            last_prompt: None,
            tag: None,
            mode: None,
            cumulative_usage: None,
            traces: Vec::new(),
            repairs: Vec::new(),
            needs_rewrite: true,
        }
    }

    fn load(path: &Path, fallback_header: Option<SessionEntry>) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut header = None;
        let mut preserved = Vec::new();
        let mut messages = Vec::new();
        let mut started = None;
        let mut summary = String::new();
        let mut custom_title = None;
        let mut ai_title = None;
        let mut last_prompt = None;
        let mut tag = None;
        let mut mode = None;
        let mut cumulative_usage = None;
        let mut repairs = Vec::new();
        let mut revision = 0;
        let mut traces = Vec::new();

        for (index, raw) in text.lines().enumerate() {
            let line_number = index + 1;
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(error) => {
                    repairs.push(SessionRepair {
                        line: Some(line_number),
                        kind: SessionRepairKind::CorruptLine,
                        detail: format!("skipped malformed JSONL line: {error}"),
                    });
                    continue;
                }
            };
            let kind = value.get("kind").and_then(|kind| kind.as_str());
            let known = matches!(
                kind,
                Some(
                    "session"
                        | "message"
                        | "summary"
                        | "custom_title"
                        | "ai_title"
                        | "last_prompt"
                        | "tag"
                        | "mode"
                        | "cumulative_usage"
                        | "trace"
                )
            );
            if !known {
                preserved.push(value);
                continue;
            }
            let parsed: SessionEntry = match serde_json::from_value(value.clone()) {
                Ok(entry) => entry,
                Err(error) => {
                    repairs.push(SessionRepair {
                        line: Some(line_number),
                        kind: SessionRepairKind::InvalidEntry,
                        detail: format!("skipped invalid {kind:?} entry: {error}"),
                    });
                    continue;
                }
            };
            match parsed {
                entry @ SessionEntry::Session { .. } => {
                    if header.is_none() {
                        if let SessionEntry::Session { started: value, .. } = &entry {
                            started = Some(value.clone());
                        }
                        header = Some(entry);
                    } else {
                        preserved.push(value);
                    }
                }
                SessionEntry::Message(message) => {
                    messages.push(message);
                    preserved.push(value);
                    revision += 1;
                }
                SessionEntry::Summary { text } => {
                    summary = text;
                    preserved.push(value);
                    revision += 1;
                }
                SessionEntry::CustomTitle { title } => {
                    custom_title = Some(title);
                    preserved.push(value);
                    revision += 1;
                }
                SessionEntry::AiTitle { title } => {
                    ai_title = Some(title);
                    preserved.push(value);
                    revision += 1;
                }
                SessionEntry::LastPrompt { prompt } => {
                    last_prompt = Some(prompt);
                    preserved.push(value);
                    revision += 1;
                }
                SessionEntry::Tag { tag: value_tag } => {
                    tag = Some(value_tag);
                    preserved.push(value);
                    revision += 1;
                }
                SessionEntry::Mode { mode: value_mode } => {
                    mode = Some(value_mode);
                    preserved.push(value);
                    revision += 1;
                }
                SessionEntry::RunOutcome { .. } => {
                    // Trajectory reward labels are append-only history: keep
                    // them in `preserved` verbatim; list_sessions counts them.
                    preserved.push(value);
                }
                SessionEntry::Trace {
                    schema_version,
                    run_id,
                    events,
                } => {
                    // Missing schema_version deserializes as legacy v1. Keep
                    // the newest complete batch for duplicate root run ids.
                    traces.retain(|existing: &PersistedTraceWire| existing.run_id != run_id);
                    traces.push(PersistedTraceWire {
                        schema_version,
                        run_id,
                        events,
                    });
                    preserved.push(value);
                }
                SessionEntry::CumulativeUsage {
                    input_tokens,
                    output_tokens,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                } => {
                    // Keep the newest CumulativeUsage entry (accumulated total).
                    cumulative_usage = Some(CumulativeUsageWire {
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                    });
                    preserved.push(value);
                    revision += 1;
                }
            }
        }

        let missing_header = header.is_none();
        let header = match header.or(fallback_header) {
            Some(header) => header,
            None => SessionEntry::Session {
                id: path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("")
                    .to_string(),
                cwd: String::new(),
                model: String::new(),
                started: chrono::Local::now().to_rfc3339(),
            },
        };
        if missing_header {
            repairs.push(SessionRepair {
                line: None,
                kind: SessionRepairKind::MissingHeader,
                detail: "session header was missing; a compatible header will be restored on the next write"
                    .into(),
            });
            if started.is_none() {
                if let SessionEntry::Session { started: value, .. } = &header {
                    started = Some(value.clone());
                }
            }
        }

        let before_repair = serde_json::to_value(&messages).ok();
        crate::loop_::repair_tool_pairing(&mut messages);
        let tool_pairing_repaired = before_repair != serde_json::to_value(&messages).ok();
        if tool_pairing_repaired {
            repairs.push(SessionRepair {
                line: None,
                kind: SessionRepairKind::ToolPairing,
                detail: "removed orphaned tool_use/tool_result content from the resumed transcript"
                    .into(),
            });
            preserved.retain(|value| {
                value.get("kind").and_then(|kind| kind.as_str()) != Some("message")
            });
            for message in &messages {
                preserved.push(serde_json::to_value(SessionEntry::Message(
                    message.clone(),
                ))?);
            }
        }

        Ok(Self {
            revision,
            header,
            preserved,
            started,
            summary,
            messages,
            custom_title,
            ai_title,
            last_prompt,
            tag,
            mode,
            cumulative_usage,
            traces,
            repairs,
            needs_rewrite: missing_header || tool_pairing_repaired,
        })
    }

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            revision: self.revision,
            started: self.started.clone(),
            summary: self.summary.clone(),
            messages: self.messages.clone(),
            title: self.title(),
            tag: self.tag.clone(),
            mode: self.mode.clone(),
            cumulative_usage: self.cumulative_usage.clone(),
            traces: self.traces.clone(),
            repairs: self.repairs.clone(),
        }
    }

    fn title(&self) -> Option<String> {
        self.custom_title
            .clone()
            .or_else(|| self.ai_title.clone())
            .or_else(|| {
                self.last_prompt
                    .as_ref()
                    .map(|prompt| prompt.chars().take(200).collect())
            })
    }

    fn append_metadata(&mut self, entry: &SessionEntry) {
        match entry {
            SessionEntry::Summary { text } => self.summary = text.clone(),
            SessionEntry::CustomTitle { title } => self.custom_title = Some(title.clone()),
            SessionEntry::AiTitle { title } => self.ai_title = Some(title.clone()),
            SessionEntry::LastPrompt { prompt } => self.last_prompt = Some(prompt.clone()),
            SessionEntry::Tag { tag } => self.tag = Some(tag.clone()),
            SessionEntry::Mode { mode } => self.mode = Some(mode.clone()),
            // RunOutcome entries are already pushed verbatim by the caller's
            // in-memory state; nothing to fold into snapshot fields.
            SessionEntry::RunOutcome { .. } => {}
            SessionEntry::Trace {
                schema_version,
                run_id,
                events,
            } => {
                self.traces.retain(|existing| existing.run_id != *run_id);
                self.traces.push(PersistedTraceWire {
                    schema_version: *schema_version,
                    run_id: run_id.clone(),
                    events: events.clone(),
                });
            }
            SessionEntry::CumulativeUsage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
            } => {
                self.cumulative_usage = Some(CumulativeUsageWire {
                    input_tokens: *input_tokens,
                    output_tokens: *output_tokens,
                    cache_creation_input_tokens: *cache_creation_input_tokens,
                    cache_read_input_tokens: *cache_read_input_tokens,
                });
            }
            SessionEntry::Session { .. } | SessionEntry::Message(_) => {}
        }
    }

    fn replace_messages(&mut self, messages: Vec<Message>) -> std::io::Result<()> {
        self.messages = messages;
        self.preserved
            .retain(|value| value.get("kind").and_then(|kind| kind.as_str()) != Some("message"));
        for message in &self.messages {
            self.preserved
                .push(serde_json::to_value(SessionEntry::Message(
                    message.clone(),
                ))?);
        }
        Ok(())
    }
}

fn writer_loop(path: PathBuf, mut state: SessionState, rx: mpsc::Receiver<SessionCommand>) {
    while let Ok(command) = rx.recv() {
        match command {
            SessionCommand::Snapshot(reply) => {
                let _ = reply.send(Ok(state.snapshot()));
            }
            SessionCommand::HistoryPage { before, limit, reply } => {
                // Pure read over the in-memory message list: messages
                // strictly before index `before`, ascending, capped at
                // `limit`. Cloning the page keeps the writer's state intact.
                let start = before.saturating_sub(limit);
                let page = state.messages[start..before.min(state.messages.len())].to_vec();
                let _ = reply.send(Ok(SessionHistoryPage {
                    revision: state.revision,
                    remaining: start,
                    messages: page,
                }));
            }
            SessionCommand::AppendMessage(message, reply) => {
                let entry = SessionEntry::Message(message.clone());
                let result = mutate_append(&path, &mut state, &entry).inspect(|_| {
                    state.messages.push(message);
                });
                let _ = reply.send(result);
            }
            SessionCommand::AppendMetadata(entry, reply) => {
                let result = mutate_append(&path, &mut state, &entry).inspect(|_| {
                    state.append_metadata(&entry);
                });
                let _ = reply.send(result);
            }
            SessionCommand::ReplaceAfterCompact {
                messages,
                expected_revision,
                reply,
            } => {
                let result = if state.revision != expected_revision {
                    Err(SessionError::RevisionConflict {
                        expected: expected_revision,
                        current: state.revision,
                    })
                } else {
                    state
                        .replace_messages(messages)
                        .map_err(SessionError::Io)
                        .and_then(|()| {
                            rewrite(&path, &state)?;
                            state.needs_rewrite = false;
                            state.revision += 1;
                            Ok(state.revision)
                        })
                };
                let _ = reply.send(result);
            }
            SessionCommand::Clear(reply) => {
                let result = (|| {
                    let cleared = SessionState {
                        revision: state.revision,
                        header: state.header.clone(),
                        preserved: Vec::new(),
                        started: state.started.clone(),
                        summary: String::new(),
                        messages: Vec::new(),
                        custom_title: None,
                        ai_title: None,
                        last_prompt: None,
                        tag: None,
                        mode: None,
                        cumulative_usage: state.cumulative_usage.clone(),
                        traces: Vec::new(),
                        repairs: state.repairs.clone(),
                        needs_rewrite: false,
                    };
                    rewrite(&path, &cleared)?;
                    let next_revision = state.revision + 1;
                    state = cleared;
                    state.revision = next_revision;
                    Ok(next_revision)
                })();
                let _ = reply.send(result);
            }
        }
    }
}

fn mutate_append(
    path: &Path,
    state: &mut SessionState,
    entry: &SessionEntry,
) -> SessionResult<u64> {
    let value = serde_json::to_value(entry)?;
    if state.needs_rewrite || !path.exists() {
        state.preserved.push(value);
        if let Err(error) = rewrite(path, state) {
            state.preserved.pop();
            return Err(error.into());
        }
        state.needs_rewrite = false;
    } else {
        append_value(path, &value)?;
        state.preserved.push(value);
    }
    state.revision += 1;
    Ok(state.revision)
}

fn append_value(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()
}

fn rewrite(path: &Path, state: &SessionState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("jsonl.tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        serde_json::to_writer(&mut file, &state.header)?;
        file.write_all(b"\n")?;
        for value in &state.preserved {
            serde_json::to_writer(&mut file, value)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

fn absolute_path(path: PathBuf) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Resolve the root directory for session storage (`$NONOCLAW_HOME` or `~/.nonoclaw`).
pub fn home_root() -> Option<PathBuf> {
    nonoclaw_core::nonoclaw_data_dir()
}

/// The per-project directory holding that cwd's sessions.
pub fn project_dir(cwd: &Path) -> Option<PathBuf> {
    let root = home_root()?;
    Some(root.join("projects").join(sanitize_cwd(cwd)))
}

/// The path of a specific session's JSONL file.
pub fn session_path(cwd: &Path, id: &str) -> Option<PathBuf> {
    Some(
        project_dir(cwd)?
            .join("sessions")
            .join(format!("{id}.jsonl")),
    )
}

/// Session-list previews are capped so one frame stays well under WebSocket
/// message limits regardless of how many sessions a project accumulated.
const LIST_SUMMARY_MAX_CHARS: usize = 240;

/// Cap a string at `max` chars (UTF-8 aware), appending an ellipsis.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn sanitize_cwd(cwd: &Path) -> String {    cwd.to_string_lossy()
        .trim_start_matches(['/', '\\'])
        .replace(['/', '\\', ':'], "-")
}

pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_core::{ContentBlock, MessageContent, Role, RunEvent};

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nonoclaw-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn open_temp() -> (PathBuf, Session) {
        let path = tempdir().join("session.jsonl");
        let session = SessionService::new()
            .open_path(path.clone(), "id-1", Path::new("/proj"), "model-x")
            .unwrap();
        (path, session)
    }

    #[test]
    fn sanitize_handles_absolute_paths() {
        assert_eq!(
            sanitize_cwd(Path::new("/home/baohx/NonoClaw")),
            "home-baohx-NonoClaw"
        );
    }

    /// Level-1 RL labels: run outcomes persist as metadata entries, are
    /// counted by list_sessions, and reward scoring matches the documented
    /// heuristic (done=1, exhaustion penalties, cancelled=-0.3, error=-1).
    #[tokio::test]
    async fn list_sessions_cache_updates_on_append() {
        let cwd = tempdir();
        let service = SessionService::new();
        let s = service.create(&cwd, "cached-session", "model-x").unwrap();
        s.append(Message::user(MessageContent::from_text("first")))
            .await
            .unwrap();

        let first = service.list_sessions(&cwd).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].message_count, 1, "message appended");

        // Second listing is served from the fingerprint cache and must agree.
        let cached = service.list_sessions(&cwd).unwrap();
        assert_eq!(cached[0].message_count, 1);

        // A later append changes the file → fingerprint differs → reparsed.
        s.append(Message::user(MessageContent::from_text("second")))
            .await
            .unwrap();
        let second = service.list_sessions(&cwd).unwrap();
        assert_eq!(
            second[0].message_count, 2,
            "cache must invalidate when the JSONL changes"
        );

        // Deleted files drop out of the cache.
        let path = s.path().to_path_buf();
        drop(s);
        std::fs::remove_file(&path).unwrap();
        let third = service.list_sessions(&cwd).unwrap();
        assert!(third.is_empty(), "deleted session still listed");
    }

    #[tokio::test]
    async fn trace_batch_roundtrips_through_disk() {
        let cwd = tempdir();
        let service = SessionService::new();
        let s = service.create(&cwd, "trace-session", "model-x").unwrap();

        let envelope = |sequence: u64, ms: u64| {
            nonoclaw_core::EventEnvelope::at(
                "run-1",
                None,
                "trace-session",
                0,
                sequence,
                ms,
                RunEvent::ThinkingState {
                    active: false,
                    turn: 1,
                },
            )
        };
        let events = vec![envelope(1, 1_000), envelope(2, 2_500)];
        s.write_trace("run-1", events)
            .await
            .unwrap();

        // Freshly read snapshot exposes the trace batch.
        let snapshot = s.snapshot().await.unwrap();
        assert_eq!(snapshot.traces.len(), 1);
        assert_eq!(snapshot.traces[0].run_id, "run-1");
        assert_eq!(snapshot.traces[0].events.len(), 2);

        // And it survives a full reopen from disk (JSONL parse).
        let path = s.path().to_path_buf();
        drop(s);
        let reopened = service.open_path(path, "trace-session", &cwd, "model-x").unwrap();
        let reopened_snapshot = reopened.snapshot().await.unwrap();
        assert_eq!(
            reopened_snapshot.traces.len(),
            1,
            "trace batch must survive reopen"
        );
        let batch = &reopened_snapshot.traces[0];
        assert_eq!(batch.run_id, "run-1");
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0].timestamp_ms, 1_000);
        assert_eq!(batch.events[1].sequence, 2);

        // A re-write for the same run_id replaces, not duplicates.
        reopened
            .write_trace("run-1", vec![envelope(1, 9_999)])
            .await
            .unwrap();
        let deduped = reopened.snapshot().await.unwrap();
        assert_eq!(deduped.traces.len(), 1);
        assert_eq!(deduped.traces[0].events.len(), 1);
        assert_eq!(deduped.traces[0].events[0].timestamp_ms, 9_999);
    }

    #[tokio::test]
    async fn run_outcome_persists_and_scores() {
        let cwd = tempdir();
        let service = SessionService::new();
        let s = service.create(&cwd, "rl-session", "model-x").unwrap();
        s.append(Message::user(MessageContent::from_text("task")))
            .await
            .unwrap();

        s.write_run_outcome("run-1", "done", 1.0, 4, "all good")
            .await
            .unwrap();
        s.write_run_outcome("run-2", "error", -1.0, 0, "provider 500")
            .await
            .unwrap();

        let infos = service.list_sessions(&cwd).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].run_outcomes, 2, "both outcome labels counted");

        // Reward heuristic anchors.
        assert_eq!(run_reward("done", "completed"), 1.0);
        assert_eq!(run_reward("done", "max turns reached"), 0.6);
        assert_eq!(run_reward("done", "budget exceeded"), 0.7);
        assert_eq!(run_reward("done", "context limit"), 0.8);
        assert_eq!(
            run_reward("done", "model stop reason: max_tokens (truncated mid-thinking; per-turn output budget exhausted before any answer)"),
            0.6,
            "mid-thinking truncation penalized like max-turns exhaustion"
        );
        assert_eq!(run_reward("cancelled", "user pressed stop"), -0.3);
        assert_eq!(run_reward("error", "boom"), -1.0);

        // Label-free adjustments (only apply to done runs).
        let ok = RewardSignals { tools_ok: 10, tools_failed: 0 };
        let mixed = RewardSignals { tools_ok: 5, tools_failed: 3 };
        let hostile = RewardSignals { tools_ok: 2, tools_failed: 5 };
        // Verification evidence bonus, capped at 1.0.
        assert_eq!(
            run_reward_labeled("done", "test result: ok. 12 passed", &RewardSignals::default()),
            1.0
        );
        assert_eq!(run_reward_labeled("done", "3 passed", &RewardSignals::default()), 1.0);
        assert_eq!(run_reward_labeled("done", "0 tests passed", &RewardSignals::default()), 1.0, "empty suite is not evidence");
        assert_eq!(run_reward_labeled("done", "completed", &RewardSignals::default()), 1.0);
        // High tool error rate penalizes even a clean finish.
        assert!((run_reward_labeled("done", "completed", &hostile) - 0.7).abs() < 1e-9);
        assert!((run_reward_labeled("done", "completed", &mixed) - 0.85).abs() < 1e-9);
        assert_eq!(run_reward_labeled("done", "completed", &ok), 1.0, "few tools → no rate judgment");
        // Non-done runs keep the base label untouched.
        assert_eq!(run_reward_labeled("error", "boom", &hostile), -1.0);
    }

    #[tokio::test]
    async fn most_recent_session_skips_dream_tagged_sessions() {
        let cwd = tempdir();
        let service = SessionService::new();

        let work = service.create(&cwd, "work-session", "model-x").unwrap();
        work.append(Message::user(MessageContent::from_text("day job")))
            .await
            .unwrap();

        // Dream runs later (newer mtime) but is machine-generated.
        let dream = service.create(&cwd, "dream-session", "model-x").unwrap();
        dream
            .append(Message::user(MessageContent::from_text("consolidating")))
            .await
            .unwrap();
        dream
            .write_tag(DREAM_SESSION_TAG)
            .await
            .unwrap();

        let picked = service.most_recent_session(&cwd).unwrap();
        assert_eq!(
            picked.as_deref(),
            Some("work-session"),
            "auto-resume must not land on a dream transcript"
        );

        // Bench-smoke harness sessions (spawned after every dream by
        // bench_validate_facts) are machine-generated the same way.
        let smoke = service
            .create(&cwd, "bench-smoke-session", "model-x")
            .unwrap();
        smoke
            .append(Message::user(MessageContent::from_text("task")))
            .await
            .unwrap();
        smoke
            .write_tag(BENCH_SMOKE_SESSION_TAG)
            .await
            .unwrap();
        assert_eq!(
            service.most_recent_session(&cwd).unwrap().as_deref(),
            Some("work-session"),
            "auto-resume must not land on a bench-smoke transcript"
        );

        // All-dream projects (fresh install, dream ran before any work)
        // resolve to None so the UI falls through to a new session.
        let only_dream_cwd = tempdir();
        let only = service
            .create(&only_dream_cwd, "dream-only", "model-x")
            .unwrap();
        only.append(Message::user(MessageContent::from_text("z")))
            .await
            .unwrap();
        only.write_tag(DREAM_SESSION_TAG).await.unwrap();
        assert_eq!(
            service.most_recent_session(&only_dream_cwd).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn write_load_roundtrip_and_metadata_compatibility() {
        let (path, session) = open_temp().await;
        session
            .append(Message::user(MessageContent::from_text("hello")))
            .await
            .unwrap();
        session
            .append(Message::assistant(MessageContent::from_text("hi there")))
            .await
            .unwrap();
        session.write_summary("summary").await.unwrap();
        session.write_ai_title("generated").await.unwrap();
        session.write_custom_title("pinned").await.unwrap();
        session.write_last_prompt("fallback").await.unwrap();
        session.write_tag("keep").await.unwrap();
        session.write_mode("plan").await.unwrap();

        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.messages[0].role, Role::User);
        assert_eq!(snapshot.messages[1].role, Role::Assistant);
        assert_eq!(snapshot.summary, "summary");
        assert_eq!(snapshot.title.as_deref(), Some("pinned"));
        assert_eq!(snapshot.tag.as_deref(), Some("keep"));
        assert_eq!(snapshot.mode.as_deref(), Some("plan"));
        assert_eq!(snapshot.revision, 8);
        assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 9);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appends_have_one_total_jsonl_order_and_monotonic_revisions() {
        // **Validates: Requirements 8.1**
        let (path, session) = open_temp().await;
        let mut tasks = Vec::new();
        for value in 0..64_u64 {
            let session = session.clone();
            tasks.push(tokio::spawn(async move {
                let revision = session
                    .append(Message::user(MessageContent::from_text(format!(
                        "message-{value}"
                    ))))
                    .await
                    .unwrap();
                (revision, value)
            }));
        }
        let mut completed = Vec::new();
        for task in tasks {
            completed.push(task.await.unwrap());
        }
        completed.sort_by_key(|(revision, _)| *revision);
        assert_eq!(
            completed
                .iter()
                .map(|(revision, _)| *revision)
                .collect::<Vec<_>>(),
            (1..=64).collect::<Vec<_>>()
        );

        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(snapshot.revision, 64);
        let ordered_values: Vec<u64> = snapshot
            .messages
            .iter()
            .map(|message| match &message.content {
                MessageContent::Text(text) => text.trim_start_matches("message-").parse().unwrap(),
                _ => panic!("expected text"),
            })
            .collect();
        assert_eq!(
            ordered_values,
            completed
                .iter()
                .map(|(_, value)| *value)
                .collect::<Vec<_>>()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 65);
    }

    #[tokio::test]
    async fn corrupt_legacy_lines_are_skipped_and_repairs_are_surfaced() {
        // **Validates: Requirements 8.5**
        let dir = tempdir();
        let path = dir.join("legacy.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"kind\":\"session\",\"id\":\"legacy\",\"cwd\":\"/proj\",\"model\":\"m\",\"started\":\"2024-01-01T00:00:00Z\"}\n",
                "not-json\n",
                "{\"kind\":\"message\",\"role\":\"user\",\"content\":\"hello\"}\n",
                "{\"kind\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"orphan\",\"name\":\"Read\",\"input\":{}}]}\n",
                "{\"kind\":\"future_entry\",\"value\":1}\n",
                "{\"kind\":\"custom_title\",\"title\":\"Pinned title\"}\n"
            ),
        )
        .unwrap();
        let session = SessionService::new()
            .open_path(path.clone(), "legacy", Path::new("/proj"), "m")
            .unwrap();
        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.title.as_deref(), Some("Pinned title"));
        assert!(snapshot
            .repairs
            .iter()
            .any(|repair| repair.kind == SessionRepairKind::CorruptLine));
        assert!(snapshot
            .repairs
            .iter()
            .any(|repair| repair.kind == SessionRepairKind::ToolPairing));

        session
            .append(Message::assistant(MessageContent::from_text("recovered")))
            .await
            .unwrap();
        let rewritten = std::fs::read_to_string(path).unwrap();
        assert!(!rewritten.contains("not-json"));
        assert!(rewritten.contains("future_entry"));
        assert!(!rewritten.contains("orphan"));
    }

    #[tokio::test]
    async fn clear_replace_and_append_are_atomic_revision_commands() {
        // **Validates: Requirements 3.7, 8.1, 8.4**
        let (path, session) = open_temp().await;
        assert_eq!(
            session
                .append(Message::user(MessageContent::from_text("before")))
                .await
                .unwrap(),
            1
        );
        let replacement = vec![Message::user(MessageContent::from_text("compacted"))];
        assert_eq!(
            session
                .replace_after_compact(replacement.clone(), 1)
                .await
                .unwrap(),
            2
        );
        assert!(matches!(
            session.replace_after_compact(Vec::new(), 1).await,
            Err(SessionError::RevisionConflict {
                expected: 1,
                current: 2
            })
        ));
        assert_eq!(
            session
                .append(Message::assistant(MessageContent::from_text("after")))
                .await
                .unwrap(),
            3
        );
        assert_eq!(session.clear().await.unwrap(), 4);
        assert_eq!(
            session
                .append(Message::user(MessageContent::from_text("fresh")))
                .await
                .unwrap(),
            5
        );

        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(snapshot.revision, 5);
        assert_eq!(snapshot.messages.len(), 1);
        assert!(matches!(
            &snapshot.messages[0].content,
            MessageContent::Text(text) if text == "fresh"
        ));
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["kind"], "session");
        assert_eq!(lines[1]["content"], "fresh");
    }

    #[tokio::test]
    async fn valid_tool_pairs_survive_repair() {
        let (path, session) = open_temp().await;
        session
            .append(Message::assistant(MessageContent::from_blocks(vec![
                ContentBlock::ToolUse {
                    id: "tool-1".into(),
                    name: "Read".into(),
                    cache_control: None,
                    input: serde_json::json!({}),
                },
            ])))
            .await
            .unwrap();
        session
            .append(Message::user(MessageContent::from_blocks(vec![
                ContentBlock::tool_result("tool-1".to_string(), "ok", false),
            ])))
            .await
            .unwrap();
        drop(session);
        let reopened = SessionService::new()
            .open_path(path, "id-1", Path::new("/proj"), "model-x")
            .unwrap();
        let snapshot = reopened.snapshot().await.unwrap();
        assert_eq!(snapshot.messages.len(), 2);
        assert!(!snapshot
            .repairs
            .iter()
            .any(|repair| repair.kind == SessionRepairKind::ToolPairing));
    }
}
