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
#[derive(Debug, Clone, Default)]
pub struct SessionService;

fn writer_registry() -> &'static Mutex<HashMap<PathBuf, Weak<SessionInner>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<SessionInner>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

impl SessionService {
    pub fn new() -> Self {
        Self
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
        for entry in read {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let mtime = entry
                .metadata()?
                .modified()
                .unwrap_or(std::time::UNIX_EPOCH);
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("")
                .to_string();
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
            let title = state.title();
            out.push(SessionInfo {
                id,
                started: state.started,
                message_count: state.messages.len(),
                summary,
                title,
                tag: state.tag,
                mtime,
            });
        }
        out.sort_by_key(|session| std::cmp::Reverse(session.mtime));
        Ok(out)
    }

    pub fn most_recent_session(&self, cwd: &Path) -> std::io::Result<Option<String>> {
        Ok(self
            .list_sessions(cwd)?
            .into_iter()
            .next()
            .map(|info| info.id))
    }
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
        let mut repairs = Vec::new();
        let mut revision = 0;

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

fn sanitize_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-")
}

pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_core::{ContentBlock, MessageContent, Role};

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
