//! Canonical, serializable technical event stream shared by every run adapter.
//!
//! Events contain observable runtime facts only. They never contain hidden
//! reasoning, complete prompts, credentials, or unbounded tool output.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ExtensionDiagnostic, TaskChange, Usage, UsagePart};

pub const EVENT_PROTOCOL_VERSION: u16 = 1;
pub const MAX_EVENT_STRING_CHARS: usize = 4_096;
pub const MAX_EVENT_ARRAY_ITEMS: usize = 128;

pub type RunId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRepairKind {
    CorruptLine,
    InvalidEntry,
    MissingHeader,
    ToolPairing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRepair {
    pub line: Option<usize>,
    pub kind: SessionRepairKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamState {
    Connecting,
    Thinking,
    Streaming,
    Completed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TechnicalStatus {
    Pending,
    Running,
    Waiting,
    Allowed,
    Denied,
    Succeeded,
    Failed,
    Cancelled,
    Truncated,
    Repaired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEvent {
    // Existing user-facing stream tags are intentionally retained.
    TextDelta {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        ok: bool,
        preview: String,
    },
    AssistantDone {
        text: String,
    },
    Compacting,
    Compacted {
        removed: usize,
        kept: usize,
        tokens_before: usize,
        tokens_after: usize,
    },
    ModelInfo {
        model: String,
    },
    SkillActivated {
        name: String,
        reason: String,
        source: String,
        version: Option<String>,
    },
    SessionRepair {
        repair: SessionRepair,
    },
    TaskChanged {
        change: TaskChange,
    },

    // Canonical technical transparency events.
    RunStarted {
        requested_model: String,
        max_turns: u32,
        max_budget_usd: Option<f64>,
    },
    ContextPrepared {
        estimated_tokens: usize,
        context_window: Option<usize>,
        tool_count: usize,
        skill_count: usize,
    },
    ModelRequestStarted {
        requested_model: String,
        provider: String,
        turn: u32,
    },
    ModelResolved {
        requested_model: String,
        actual_model: String,
        provider: String,
        turn: u32,
    },
    ProviderDiagnostic {
        provider: String,
        category: String,
        status: TechnicalStatus,
        detail: String,
    },
    StreamStateChanged {
        state: StreamState,
        turn: u32,
    },
    ThinkingState {
        active: bool,
        turn: u32,
    },
    RetryScheduled {
        attempt: u32,
        delay_ms: u64,
        category: String,
        operation: String,
    },
    ToolQueued {
        tool_use_id: String,
        tool_name: String,
        index: usize,
    },
    ToolValidation {
        tool_use_id: String,
        tool_name: String,
        ok: bool,
        detail: String,
    },
    PermissionRequested {
        tool_use_id: String,
        tool_name: String,
        waiting_on: String,
    },
    PermissionResolved {
        tool_use_id: String,
        tool_name: String,
        decision: TechnicalStatus,
        elapsed_ms: u64,
    },
    ToolExecutionStarted {
        tool_use_id: String,
        tool_name: String,
        read_only: Option<bool>,
        destructive: Option<bool>,
    },
    ToolExecutionFinished {
        tool_use_id: String,
        tool_name: String,
        status: TechnicalStatus,
        elapsed_ms: u64,
    },
    ToolResultNormalized {
        tool_use_id: String,
        original_chars: usize,
        visible_chars: usize,
        truncated: bool,
        local_reference: Option<String>,
    },
    HookStarted {
        hook_type: String,
        action: String,
        matcher: String,
    },
    HookFinished {
        hook_type: String,
        action: String,
        matcher: String,
        status: TechnicalStatus,
        elapsed_ms: u64,
    },
    SubagentStarted {
        description: String,
    },
    SubagentFinished {
        description: String,
        status: TechnicalStatus,
        elapsed_ms: u64,
    },
    BackgroundTaskChanged {
        task_id: String,
        status: TechnicalStatus,
        exit_code: Option<i32>,
    },
    CompactionStarted {
        automatic: bool,
        tokens_before: usize,
        messages_before: usize,
    },
    RecoveryApplied {
        category: String,
        detail: String,
        items_affected: usize,
    },
    ExtensionDiagnostic {
        diagnostic: ExtensionDiagnostic,
    },
    McpDiagnostic {
        server: String,
        status: TechnicalStatus,
        source: Option<String>,
        detail: String,
    },
    ConfigDiagnostic {
        severity: String,
        code: String,
        field: Option<String>,
        source: Option<String>,
        message: String,
        suggestion: String,
    },
    UsageUpdated {
        turn: u32,
        turn_usage: UsagePart,
        total: Usage,
        max_budget_usd: Option<f64>,
    },
    CancellationRequested {
        reason: String,
    },
    RunError {
        code: String,
        operation: String,
        retryable: bool,
        message: String,
    },
    RunFinished {
        status: TechnicalStatus,
        reason: String,
        duration_ms: u64,
        turns: u32,
        usage: Usage,
    },
}

impl RunEvent {
    /// Return a bounded, field-redacted copy suitable for trace and transport.
    pub fn redacted(&self) -> Self {
        let value = serde_json::to_value(self).unwrap_or(Value::Null);
        serde_json::from_value(redact_value(value)).unwrap_or_else(|_| RunEvent::RunError {
            code: "event_serialization".into(),
            operation: "redact_event".into(),
            retryable: false,
            message: "event payload could not be safely serialized".into(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope<T = RunEvent> {
    pub protocol_version: u16,
    pub event_id: String,
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,
    pub session_id: String,
    pub session_revision: u64,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub event: T,
}

impl EventEnvelope<RunEvent> {
    pub fn new(
        run_id: impl Into<String>,
        parent_run_id: Option<String>,
        session_id: impl Into<String>,
        session_revision: u64,
        sequence: u64,
        event: RunEvent,
    ) -> Self {
        Self::at(
            run_id,
            parent_run_id,
            session_id,
            session_revision,
            sequence,
            timestamp_ms(),
            event,
        )
    }

    pub fn at(
        run_id: impl Into<String>,
        parent_run_id: Option<String>,
        session_id: impl Into<String>,
        session_revision: u64,
        sequence: u64,
        timestamp_ms: u64,
        event: RunEvent,
    ) -> Self {
        Self {
            protocol_version: EVENT_PROTOCOL_VERSION,
            event_id: uuid::Uuid::new_v4().to_string(),
            run_id: run_id.into(),
            parent_run_id,
            session_id: session_id.into(),
            session_revision,
            sequence,
            timestamp_ms,
            event: event.redacted(),
        }
    }

    pub fn with_session_revision(mut self, revision: u64) -> Self {
        self.session_revision = revision;
        self
    }
}

pub fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn redact_value(value: Value) -> Value {
    redact_value_inner(value, 0)
}

fn redact_value_inner(value: Value, depth: usize) -> Value {
    if depth > 16 {
        return Value::String("[TRUNCATED]".into());
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(&key) {
                        Value::String("[REDACTED]".into())
                    } else {
                        redact_value_inner(value, depth + 1)
                    };
                    (key, value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .take(MAX_EVENT_ARRAY_ITEMS)
                .map(|value| redact_value_inner(value, depth + 1))
                .collect(),
        ),
        Value::String(value) => Value::String(redact_string(&value)),
        other => other,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "apikey",
        "authtoken",
        "authorization",
        "credential",
        "password",
        "secret",
        "token",
        "prompt",
        "promptpreview",
        "attachmentdata",
        "extractedtext",
        "content",
        "body",
        "image",
        "images",
        "imagedata",
        "imagebase64",
        "data",
        "thinking",
        "signature",
    ]
    .iter()
    .any(|sensitive| key == *sensitive || key.ends_with(sensitive))
}

pub fn redact_text(value: &str) -> String {
    redact_string(value)
}

fn redact_string(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if lower.contains("bearer ")
        || lower.contains("authorization:")
        || lower.contains("\"authorization\"")
        || lower.contains("x-api-key")
        || lower.contains("api_key=")
        || lower.contains("apikey=")
        || lower.contains("\"api_key\"")
        || lower.contains("\"apikey\"")
        || lower.contains("password=")
        || lower.contains("\"password\"")
        || lower.contains("secret=")
        || lower.contains("\"secret\"")
        || lower.contains("sk-ant-")
        || lower.contains("sk-proj-")
        || lower.contains("/home/")
        || lower.contains("/users/")
        || lower.contains("c:\\users\\")
    {
        return "[REDACTED]".into();
    }
    let mut bounded: String = value.chars().take(MAX_EVENT_STRING_CHARS).collect();
    if value.chars().count() > MAX_EVENT_STRING_CHARS {
        bounded.push_str("…[truncated]");
    }
    bounded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_serialization_has_stable_identity_and_order_metadata() {
        // **Validates: Requirements 2.5, 9.1, 9.7**
        let envelope = EventEnvelope::at(
            "run-1",
            Some("parent-1".into()),
            "session-1",
            7,
            42,
            1_700_000_000_000,
            RunEvent::StreamStateChanged {
                state: StreamState::Streaming,
                turn: 2,
            },
        );
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["protocol_version"], EVENT_PROTOCOL_VERSION);
        assert_eq!(value["run_id"], "run-1");
        assert_eq!(value["parent_run_id"], "parent-1");
        assert_eq!(value["session_id"], "session-1");
        assert_eq!(value["session_revision"], 7);
        assert_eq!(value["sequence"], 42);
        assert_eq!(value["timestamp_ms"], 1_700_000_000_000_u64);
        assert_eq!(value["event"]["kind"], "stream_state_changed");
        assert!(!value["event_id"].as_str().unwrap().is_empty());
        let decoded: EventEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.sequence, 42);
    }

    #[test]
    fn redaction_is_recursive_bounded_and_preserves_safe_fields() {
        // **Validates: Requirements 9.7, 9.8**
        let event = RunEvent::ToolUseStart {
            id: "tool-1".into(),
            name: "Example".into(),
            input: json!({
                "path": "src/main.rs",
                "api_key": "top-secret",
                "nested": {"authorization": "Bearer token"},
                "content": "private prompt body",
                "image": {"data": "private attachment bytes"},
                "thinking": "hidden model reasoning",
                "signature": "provider reasoning signature",
                "output": "x".repeat(MAX_EVENT_STRING_CHARS + 100),
            }),
        }
        .redacted();
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["input"]["path"], "src/main.rs");
        assert_eq!(value["input"]["api_key"], "[REDACTED]");
        assert_eq!(value["input"]["nested"]["authorization"], "[REDACTED]");
        for field in ["content", "image", "thinking", "signature"] {
            assert_eq!(value["input"][field], "[REDACTED]", "leaked {field}");
        }
        assert!(value["input"]["output"]
            .as_str()
            .unwrap()
            .ends_with("…[truncated]"));
    }

    #[test]
    fn event_category_contract_has_representatives_for_all_required_domains() {
        // **Validates: Requirements 9.1, 9.3, 9.4, 9.5, 9.6**
        let events = vec![
            RunEvent::RunStarted {
                requested_model: "m".into(),
                max_turns: 1,
                max_budget_usd: None,
            },
            RunEvent::ModelRequestStarted {
                requested_model: "m".into(),
                provider: "p".into(),
                turn: 1,
            },
            RunEvent::ProviderDiagnostic {
                provider: "p".into(),
                category: "streaming".into(),
                status: TechnicalStatus::Succeeded,
                detail: "supported".into(),
            },
            RunEvent::ToolQueued {
                tool_use_id: "t".into(),
                tool_name: "Read".into(),
                index: 0,
            },
            RunEvent::PermissionRequested {
                tool_use_id: "t".into(),
                tool_name: "Read".into(),
                waiting_on: "user".into(),
            },
            RunEvent::ToolExecutionFinished {
                tool_use_id: "t".into(),
                tool_name: "Read".into(),
                status: TechnicalStatus::Succeeded,
                elapsed_ms: 2,
            },
            RunEvent::ToolResultNormalized {
                tool_use_id: "t".into(),
                original_chars: 10,
                visible_chars: 10,
                truncated: false,
                local_reference: None,
            },
            RunEvent::HookStarted {
                hook_type: "PreToolUse".into(),
                action: "command".into(),
                matcher: "Read".into(),
            },
            RunEvent::TaskChanged {
                change: TaskChange {
                    scope: "s".into(),
                    source: crate::TaskChangeSource::TaskUpdate,
                    change: crate::TaskChangeKind::Updated,
                    tasks: vec![],
                },
            },
            RunEvent::SubagentStarted {
                description: "child".into(),
            },
            RunEvent::BackgroundTaskChanged {
                task_id: "bg".into(),
                status: TechnicalStatus::Running,
                exit_code: None,
            },
            RunEvent::CompactionStarted {
                automatic: true,
                tokens_before: 10,
                messages_before: 2,
            },
            RunEvent::SessionRepair {
                repair: SessionRepair {
                    line: Some(1),
                    kind: SessionRepairKind::CorruptLine,
                    detail: "skipped".into(),
                },
            },
            RunEvent::ExtensionDiagnostic {
                diagnostic: ExtensionDiagnostic {
                    severity: crate::ExtensionDiagnosticSeverity::Warning,
                    code: "extension_conflict".into(),
                    kind: crate::ExtensionKind::Skill,
                    name: Some("fixture".into()),
                    source: Some("project".into()),
                    message: "shadowed".into(),
                    suggestion: "rename it".into(),
                },
            },
            RunEvent::McpDiagnostic {
                server: "local".into(),
                status: TechnicalStatus::Pending,
                source: None,
                detail: "configured".into(),
            },
            RunEvent::ConfigDiagnostic {
                severity: "warning".into(),
                code: "c".into(),
                field: None,
                source: None,
                message: "m".into(),
                suggestion: "s".into(),
            },
            RunEvent::UsageUpdated {
                turn: 1,
                turn_usage: UsagePart::default(),
                total: Usage::default(),
                max_budget_usd: None,
            },
            RunEvent::CancellationRequested {
                reason: "user".into(),
            },
            RunEvent::RunError {
                code: "provider_stream".into(),
                operation: "stream_turn".into(),
                retryable: true,
                message: "interrupted".into(),
            },
            RunEvent::RunFinished {
                status: TechnicalStatus::Succeeded,
                reason: "done".into(),
                duration_ms: 1,
                turns: 1,
                usage: Usage::default(),
            },
        ];
        for (index, event) in events.into_iter().enumerate() {
            let envelope =
                EventEnvelope::at("r", None, "s", 0, index as u64 + 1, index as u64, event);
            let encoded = serde_json::to_vec(&envelope).unwrap();
            let decoded: EventEnvelope = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded.sequence, index as u64 + 1);
        }
    }

    #[test]
    fn ordering_metadata_is_strict_for_broad_sequence_range() {
        // Property-style check over the valid sequence domain used by adapters.
        let envelopes = (1_u64..=2_048)
            .map(|sequence| {
                EventEnvelope::at(
                    "r",
                    None,
                    "s",
                    0,
                    sequence,
                    sequence,
                    RunEvent::ThinkingState {
                        active: true,
                        turn: 1,
                    },
                )
            })
            .collect::<Vec<_>>();
        assert!(envelopes
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
        assert!(envelopes
            .iter()
            .all(|event| event.run_id == "r" && event.session_id == "s"));
    }
}
