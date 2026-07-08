//! Permission types. Mirrors `src/types/permissions.ts`.
//!
//! The gating flow (in the engine) is:
//!   `validate_input` -> `check_permissions` -> (rule/mode decision or
//!   interactive prompt) -> `call`. See `src/Tool.ts` and `src/utils/permissions/`.

use serde::{Deserialize, Serialize};

/// Coarse permission posture for the session. Mirrors the TS `PermissionMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Prompt for every non-allowlisted tool use.
    #[default]
    Default,
    /// Auto-approve file edits, still prompt for others.
    AcceptEdits,
    /// Auto-approve everything the rule engine / classifier permits.
    Auto,
    /// Skip all permission prompts (the `--dangerously-skip-permissions` flag).
    BypassPermissions,
    /// Read-only planning posture.
    Plan,
}

impl PermissionMode {
    pub fn from_kebab(s: &str) -> Option<Self> {
        Some(match s {
            "default" => PermissionMode::Default,
            "acceptEdits" | "accept-edits" => PermissionMode::AcceptEdits,
            "auto" => PermissionMode::Auto,
            "bypassPermissions" | "bypass-permissions" => PermissionMode::BypassPermissions,
            "plan" => PermissionMode::Plan,
            _ => return None,
        })
    }
}

/// Outcome of `check_permissions`. `Ask` means the engine must surface a prompt
/// (in headless mode without bypass, an `Ask` becomes a denial).
#[derive(Debug, Clone)]
pub enum PermissionDecision {
    /// Allowed. May carry an amended input (e.g. a hook rewrote the command).
    Allow {
        updated_input: Option<serde_json::Value>,
    },
    /// Hard denial; the tool is not run. `reason` is reported to the model.
    Deny { reason: String },
    /// Needs the user to decide.
    Ask { message: String },
}

impl PermissionDecision {
    pub fn allow() -> Self {
        PermissionDecision::Allow {
            updated_input: None,
        }
    }
    pub fn deny<S: Into<String>>(reason: S) -> Self {
        PermissionDecision::Deny {
            reason: reason.into(),
        }
    }
    pub fn ask<S: Into<String>>(message: S) -> Self {
        PermissionDecision::Ask {
            message: message.into(),
        }
    }
    pub fn is_allow(&self) -> bool {
        matches!(self, PermissionDecision::Allow { .. })
    }
}

/// Convenience alias: a tool's permission check yields a decision.
pub type PermissionResult = PermissionDecision;

/// Outcome of `validate_input` — runs before permission checks. Mirrors
/// `ValidationResult` in `src/Tool.ts`.
#[derive(Debug, Clone)]
pub enum ValidationResult {
    Ok,
    Invalid { message: String, code: u32 },
}

impl ValidationResult {
    pub fn ok() -> Self {
        ValidationResult::Ok
    }
    pub fn invalid<S: Into<String>>(message: S) -> Self {
        ValidationResult::Invalid {
            message: message.into(),
            code: 1,
        }
    }
    pub fn is_ok(&self) -> bool {
        matches!(self, ValidationResult::Ok)
    }
}
