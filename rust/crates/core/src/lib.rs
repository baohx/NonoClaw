//! Core types for NonoClaw: messages, content blocks, usage, permissions, errors.
//! Pure data — no I/O. Mirrors `src/types/` in the TS reference (some files,
//! e.g. `message.ts`, are absent from this extraction and reconstructed from
//! usage in `src/Tool.ts`, `src/query.ts`, `src/services/api/claude.ts`).

pub mod error;
pub mod message;
pub mod permissions;
pub mod usage;

pub use error::{ApiErrorKind, Error, Result};
pub use message::{
    CacheControl, CacheControlKind, ContentBlock, ImageSource, Message, MessageContent, Role,
    StopReason, ToolResultContent,
};
pub use permissions::{PermissionDecision, PermissionMode, PermissionResult, ValidationResult};
pub use usage::{Usage, UsagePart};
