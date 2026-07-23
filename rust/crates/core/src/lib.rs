//! Core types for NonoClaw: messages, content blocks, usage, permissions, errors.
//! Pure data — no I/O. Mirrors `src/types/` in the TS reference (some files,
//! e.g. `message.ts`, are absent from this extraction and reconstructed from
//! usage in `src/Tool.ts`, `src/query.ts`, `src/services/api/claude.ts`).

pub mod error;
pub mod extension;
pub mod message;
pub mod permissions;
pub mod run_event;
pub mod task;
pub mod usage;

pub use error::{ApiErrorKind, AppError, Error, ErrorCode, Result};
pub use extension::{
    resolve_extension_conflicts, ExtensionDescriptor, ExtensionDiagnostic,
    ExtensionDiagnosticSeverity, ExtensionKind, ExtensionSourceKind, ExtensionStatus,
};
pub use message::{
    CacheControl, CacheControlKind, ContentBlock, ImageSource, Message, MessageContent, Role,
    StopReason, ToolResultContent,
};
pub use permissions::{PermissionDecision, PermissionMode, PermissionResult, ValidationResult};
pub use run_event::{
    redact_text, redact_value, timestamp_ms, EventEnvelope, RunEvent, RunId, SessionRepair,
    SessionRepairKind, StreamState, TechnicalStatus, EVENT_PROTOCOL_VERSION,
};
pub use task::{TaskChange, TaskChangeKind, TaskChangeSource, TaskSnapshot, TaskStatus};
pub use usage::{Usage, UsagePart};

/// The user's home directory (platform-aware).
/// `$HOME` on Unix / Git Bash; `%USERPROFILE%` on Windows cmd.
pub fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        if let Some(h) = std::env::var_os("HOME") {
            return Some(std::path::PathBuf::from(h));
        }
        std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }
}

/// Resolve the nonoclaw data directory for config / sessions / plugins.
/// `$NONOCLAW_HOME` → `$HOME/.nonoclaw` (Unix / Git Bash) →
/// `%USERPROFILE%\.nonoclaw` (Windows cmd).
pub fn nonoclaw_data_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("NONOCLAW_HOME") {
        return Some(std::path::PathBuf::from(d));
    }
    home_dir().map(|h| h.join(".nonoclaw"))
}
