//! Shared task-domain types used by task tools and structured run events.

use serde::{Deserialize, Serialize};

/// Canonical lifecycle shared by TodoWrite and Task* tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    /// Existing TodoWrite and TaskUpdate contracts allow moving between any
    /// two declared states (including reopening completed work). Keeping this
    /// policy explicit prevents the two tool families from drifting again.
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Pending,
                Self::Pending | Self::InProgress | Self::Completed
            ) | (
                Self::InProgress,
                Self::Pending | Self::InProgress | Self::Completed
            ) | (
                Self::Completed,
                Self::Pending | Self::InProgress | Self::Completed
            )
        )
    }

    pub fn transition_to(self, next: Self) -> Self {
        debug_assert!(self.can_transition_to(next));
        next
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Completed => write!(f, "completed"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskChangeSource {
    TodoWrite,
    TaskCreate,
    TaskUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskChangeKind {
    Replaced,
    Created,
    Updated,
}

/// Stable, presentation-safe task projection carried by run events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub id: String,
    pub subject: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

/// Structured mutation emitted by the canonical task store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskChange {
    /// Todo scope (normally a session/agent id), or `task_graph` for Task*.
    pub scope: String,
    pub source: TaskChangeSource,
    pub change: TaskChangeKind,
    pub tasks: Vec<TaskSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_state_machine_is_closed_and_serializes_canonically() {
        // **Validates: Requirements 1.4, 2.3**
        let states = [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
        ];
        for from in states {
            for to in states {
                assert!(from.can_transition_to(to));
                assert_eq!(from.transition_to(to), to);
                let encoded = serde_json::to_string(&to).unwrap();
                let decoded: TaskStatus = serde_json::from_str(&encoded).unwrap();
                assert_eq!(decoded, to);
            }
        }
        assert_eq!(TaskStatus::parse("pending"), Some(TaskStatus::Pending));
        assert_eq!(TaskStatus::parse("invalid"), None);
    }
}
