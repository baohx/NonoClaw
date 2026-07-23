//! Browser/server wire protocol and serialization helpers.
//!
//! This module is the sole owner of WebSocket tags and fields. Keep changes
//! synchronized with `frontend/src/protocol-fixtures.json`.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{SinkExt, StreamExt};
use nonoclaw_core::{
    redact_text, redact_value, AppError, ContentBlock, ErrorCode, Message, MessageContent,
    ToolResultContent,
};
use nonoclaw_engine::{
    EventEnvelope, RunEvent, RunTerminal, SequencedEngineEvent, Session, SessionSnapshot,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::project_info::ProjectInfo;

pub(super) type Tx = std::sync::Arc<Mutex<futures::stream::SplitSink<WebSocket, WsMessage>>>;
pub(super) const WS_PROTOCOL_VERSION: u16 = nonoclaw_core::EVENT_PROTOCOL_VERSION;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ClientMsg {
    Run {
        prompt: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        max_turns: Option<u32>,
        #[serde(default)]
        append_system_prompt: Option<String>,
        #[serde(default)]
        arguments: Option<String>,
        #[serde(default)]
        attachments: Option<Vec<AttachmentRef>>,
    },
    Cancel,
    Clear,
    NewSession,
    ResumeSession {
        id: String,
    },
    Compact,
    PermissionDecision {
        request_id: String,
        decision: String,
    },
    QuestionAnswer {
        request_id: String,
        #[serde(default)]
        answer: Option<String>,
    },
    FileTree,
    OpenFile {
        path: String,
        #[serde(default)]
        force_code: bool,
    },
    ProjectInfoRefresh,
    GitShow {
        sha: String,
    },
    SetPermissionMode {
        mode: String,
    },
    SetModel {
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AttachmentRef {
    pub id: String,
    pub filename: String,
    pub extracted_text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ImageRef {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Serialize)]
pub(super) struct UploadResponse {
    pub id: String,
    pub filename: String,
    pub extracted_text: String,
    pub image_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageRef>>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub(super) struct ModelInfo {
    pub name: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
}

#[derive(Debug, Serialize, Clone)]
pub(super) struct SessionInfoWire {
    pub id: String,
    pub started: Option<String>,
    pub message_count: usize,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ServerMsg {
    Event {
        #[serde(flatten)]
        envelope: EventEnvelope,
    },
    PermissionRequired {
        request_id: String,
        tool_name: String,
        message: String,
        input: serde_json::Value,
    },
    QuestionRequired {
        request_id: String,
        prompt: String,
        options: Vec<String>,
    },
    Done {
        protocol_version: u16,
        run_id: String,
        session_id: String,
        session_revision: u64,
        sequence: u64,
        timestamp_ms: u64,
        text: String,
        usage: serde_json::Value,
        turns: u32,
        stop_reason: Option<String>,
    },
    Error {
        #[serde(flatten)]
        error: AppError,
    },
    #[serde(rename = "error")]
    RunError {
        protocol_version: u16,
        run_id: String,
        session_id: String,
        session_revision: u64,
        sequence: u64,
        timestamp_ms: u64,
        #[serde(flatten)]
        error: AppError,
    },
    Info {
        model: String,
        session_id: String,
        auth_token: String,
        available_models: Vec<ModelInfo>,
    },
    SessionList {
        sessions: Vec<SessionInfoWire>,
    },
    MessagesLoaded {
        protocol_version: u16,
        session_id: String,
        revision: u64,
        timestamp_ms: u64,
        messages: Vec<serde_json::Value>,
    },
    FileTree {
        root: String,
        entries: Vec<FileEntry>,
    },
    ProjectInfo {
        info: ProjectInfo,
    },
    GitShow {
        sha: String,
        output: String,
    },
}

#[derive(Debug, Serialize, Clone)]
pub(super) struct FileEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub depth: u32,
}

pub(super) fn safe_error(
    code: ErrorCode,
    message: impl Into<String>,
    retryable: bool,
    operation: impl Into<String>,
) -> ServerMsg {
    ServerMsg::Error {
        error: AppError::new(code, message, retryable, operation)
            .with_trace_id(uuid::Uuid::new_v4().to_string()),
    }
}

pub(super) fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(super) fn messages_loaded(session_id: &str, snapshot: SessionSnapshot) -> ServerMsg {
    ServerMsg::MessagesLoaded {
        protocol_version: WS_PROTOCOL_VERSION,
        session_id: session_id.to_string(),
        revision: snapshot.revision,
        timestamp_ms: timestamp_ms(),
        messages: snapshot
            .messages
            .into_iter()
            .map(message_for_wire)
            .collect(),
    }
}

/// Convert persisted model messages to the browser compatibility shape while
/// removing data that the UI never needs: provider thinking/signatures,
/// attachment bytes/extracted bodies, and unsafe tool payload fields.
fn message_for_wire(message: Message) -> serde_json::Value {
    serde_json::json!({
        "role": message.role,
        "content": safe_message_content(message.content),
    })
}

fn safe_message_content(content: MessageContent) -> serde_json::Value {
    match content {
        MessageContent::Text(text) => serde_json::Value::String(text),
        MessageContent::Blocks(blocks) => {
            if let Some(prompt) = attached_user_prompt(&blocks) {
                return serde_json::json!([{
                    "type": "text",
                    "text": format!("[attachment content kept server-side]\n\n{prompt}")
                }]);
            }
            serde_json::Value::Array(blocks.into_iter().filter_map(safe_block).collect())
        }
    }
}

fn attached_user_prompt(blocks: &[ContentBlock]) -> Option<String> {
    const ATTACHMENT_HEADER: &str = "The user has attached the following files.";
    const USER_MESSAGE_HEADER: &str = "---\n\n## User message\n\n";
    let is_attachment_message = blocks.iter().any(|block| {
        matches!(block, ContentBlock::Image { .. })
            || matches!(block, ContentBlock::Text { text, .. } if text.starts_with(ATTACHMENT_HEADER))
    });
    if !is_attachment_message {
        return None;
    }
    Some(
        blocks
            .iter()
            .rev()
            .find_map(|block| match block {
                ContentBlock::Text { text, .. } => text
                    .strip_prefix(USER_MESSAGE_HEADER)
                    .map(ToOwned::to_owned),
                _ => None,
            })
            .unwrap_or_default(),
    )
}

fn safe_block(block: ContentBlock) -> Option<serde_json::Value> {
    match block {
        ContentBlock::Text { text, .. } => Some(serde_json::json!({
            "type": "text",
            "text": text,
        })),
        ContentBlock::Image { .. } => Some(serde_json::json!({
            "type": "text",
            "text": "[attachment image kept server-side]",
        })),
        ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": redact_value(input),
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Some(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": safe_tool_result_content(content),
            "is_error": is_error,
        })),
        ContentBlock::Thinking { .. } => None,
    }
}

fn safe_tool_result_content(content: ToolResultContent) -> serde_json::Value {
    match content {
        ToolResultContent::Text(text) => serde_json::Value::String(redact_text(&text)),
        ToolResultContent::Blocks(blocks) => {
            serde_json::Value::Array(blocks.into_iter().filter_map(safe_block).collect())
        }
    }
}

pub(super) async fn event_message(session: &Session, sequenced: SequencedEngineEvent) -> ServerMsg {
    let session_revision = session
        .snapshot()
        .await
        .map(|snapshot| snapshot.revision)
        .unwrap_or_default();
    ServerMsg::Event {
        envelope: sequenced.with_session_revision(session_revision),
    }
}

pub(super) fn synthetic_event_message(
    run_id: &str,
    session_id: &str,
    session_revision: u64,
    sequence: u64,
    event: RunEvent,
) -> ServerMsg {
    ServerMsg::Event {
        envelope: EventEnvelope::new(run_id, None, session_id, session_revision, sequence, event),
    }
}

pub(super) fn terminal_fields(
    terminal: &RunTerminal,
    session_revision: u64,
) -> (u16, String, String, u64, u64, u64) {
    (
        WS_PROTOCOL_VERSION,
        terminal.run_id.clone(),
        terminal.session_id.clone(),
        session_revision,
        terminal.sequence,
        timestamp_ms(),
    )
}

pub(super) async fn send_msg(tx: &Tx, msg: ServerMsg) {
    if !send_msg_ok(tx, &msg).await {
        tracing::warn!("websocket send failed");
    }
}

pub(super) async fn send_msg_ok(tx: &Tx, msg: &ServerMsg) -> bool {
    let Ok(text) = serde_json::to_string(msg) else {
        return false;
    };
    tx.lock().await.send(WsMessage::Text(text)).await.is_ok()
}

pub(super) fn split_socket(socket: WebSocket) -> (Tx, futures::stream::SplitStream<WebSocket>) {
    let (tx, rx) = socket.split();
    (std::sync::Arc::new(Mutex::new(tx)), rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_core::{ImageSource, Role};

    #[test]
    fn session_wire_messages_hide_thinking_attachments_and_sensitive_tool_fields() {
        // **Validates: Requirements 8.8, 9.8, 11.1**
        let attachment = Message::user(MessageContent::from_blocks(vec![
            ContentBlock::text(
                "The user has attached the following files. Their content has already been extracted.",
            ),
            ContentBlock::text("private attachment body sk-proj-attachment"),
            ContentBlock::Image {
                source: ImageSource {
                    kind: "base64".into(),
                    media_type: "image/png".into(),
                    data: "private-image-data".into(),
                },
            },
            ContentBlock::text("---\n\n## User message\n\nplease summarize"),
        ]));
        let assistant = Message {
            role: Role::Assistant,
            content: MessageContent::from_blocks(vec![
                ContentBlock::Thinking {
                    thinking: "hidden chain of thought".into(),
                    signature: Some("provider-signature".into()),
                },
                ContentBlock::ToolUse {
                    id: "tool-1".into(),
                    name: "Fixture".into(),
                    input: serde_json::json!({
                        "path": "src/main.rs",
                        "api_key": "sk-proj-tool",
                        "content": "private tool prompt"
                    }),
                },
                ContentBlock::text("visible answer"),
            ]),
        };

        let encoded =
            serde_json::json!([message_for_wire(attachment), message_for_wire(assistant)])
                .to_string();
        for forbidden in [
            "private attachment body",
            "private-image-data",
            "hidden chain of thought",
            "provider-signature",
            "sk-proj-tool",
            "private tool prompt",
        ] {
            assert!(!encoded.contains(forbidden), "wire leaked {forbidden}");
        }
        assert!(encoded.contains("attachment content kept server-side"));
        assert!(encoded.contains("please summarize"));
        assert!(encoded.contains("visible answer"));
        assert!(encoded.contains("src/main.rs"));
    }

    fn client_kind(message: ClientMsg) -> &'static str {
        match message {
            ClientMsg::Run { .. } => "run",
            ClientMsg::Cancel => "cancel",
            ClientMsg::Clear => "clear",
            ClientMsg::NewSession => "new_session",
            ClientMsg::ResumeSession { .. } => "resume_session",
            ClientMsg::Compact => "compact",
            ClientMsg::PermissionDecision { .. } => "permission_decision",
            ClientMsg::QuestionAnswer { .. } => "question_answer",
            ClientMsg::FileTree => "file_tree",
            ClientMsg::OpenFile { .. } => "open_file",
            ClientMsg::ProjectInfoRefresh => "project_info_refresh",
            ClientMsg::GitShow { .. } => "git_show",
            ClientMsg::SetPermissionMode { .. } => "set_permission_mode",
            ClientMsg::SetModel { .. } => "set_model",
        }
    }

    #[test]
    fn all_client_tags_are_stable() {
        let fixtures = [
            (
                r#"{"type":"run","prompt":"hello","model":"fixture-model","max_turns":1,"append_system_prompt":"extra","arguments":"arg","attachments":[{"id":"a","filename":"a.txt","extracted_text":"body","images":[]}]}"#,
                "run",
            ),
            (r#"{"type":"cancel"}"#, "cancel"),
            (r#"{"type":"clear"}"#, "clear"),
            (r#"{"type":"new_session"}"#, "new_session"),
            (
                r#"{"type":"resume_session","id":"abc-123"}"#,
                "resume_session",
            ),
            (r#"{"type":"compact"}"#, "compact"),
            (
                r#"{"type":"permission_decision","request_id":"p1","decision":"allow"}"#,
                "permission_decision",
            ),
            (
                r#"{"type":"question_answer","request_id":"q1","answer":"yes"}"#,
                "question_answer",
            ),
            (r#"{"type":"file_tree"}"#, "file_tree"),
            (
                r#"{"type":"open_file","path":"src/main.rs","force_code":true}"#,
                "open_file",
            ),
            (r#"{"type":"project_info_refresh"}"#, "project_info_refresh"),
            (r#"{"type":"git_show","sha":"abc123"}"#, "git_show"),
            (
                r#"{"type":"set_permission_mode","mode":"plan"}"#,
                "set_permission_mode",
            ),
            (
                r#"{"type":"set_model","name":"fixture-model"}"#,
                "set_model",
            ),
        ];
        for (json, expected) in fixtures {
            assert_eq!(client_kind(serde_json::from_str(json).unwrap()), expected);
        }
    }

    #[test]
    fn rust_and_typescript_share_checked_protocol_fixtures() {
        // **Validates: Requirements 2.5, 8.2, 8.6**
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../../frontend/src/protocol-fixtures.json"
        ))
        .unwrap();
        let mut envelope = EventEnvelope::at(
            "run-fixture",
            None,
            "session-fixture",
            7,
            3,
            1_700_000_000_000,
            RunEvent::TextDelta {
                text: "hello".into(),
            },
        );
        envelope.event_id = "event-fixture".into();
        let event = ServerMsg::Event { envelope };
        let snapshot = ServerMsg::MessagesLoaded {
            protocol_version: 1,
            session_id: "session-fixture".into(),
            revision: 7,
            timestamp_ms: 1_700_000_000_001,
            messages: vec![],
        };
        let done = ServerMsg::Done {
            protocol_version: 1,
            run_id: "run-fixture".into(),
            session_id: "session-fixture".into(),
            session_revision: 8,
            sequence: 4,
            timestamp_ms: 1_700_000_000_002,
            text: "hello".into(),
            usage: serde_json::json!({"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}),
            turns: 1,
            stop_reason: Some("end_turn".into()),
        };
        let error = ServerMsg::RunError {
            protocol_version: 1,
            run_id: "run-fixture".into(),
            session_id: "session-fixture".into(),
            session_revision: 8,
            sequence: 4,
            timestamp_ms: 1_700_000_000_002,
            error: AppError::new(ErrorCode::Internal, "fixture failure", false, "fixture")
                .with_trace_id("trace-fixture"),
        };
        assert_eq!(serde_json::to_value(event).unwrap(), fixtures["event"]);
        assert_eq!(
            serde_json::to_value(snapshot).unwrap(),
            fixtures["snapshot"]
        );
        assert_eq!(serde_json::to_value(done).unwrap(), fixtures["done"]);
        assert_eq!(serde_json::to_value(error).unwrap(), fixtures["error"]);
    }
}
