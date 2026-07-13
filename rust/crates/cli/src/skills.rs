//! Skill discovery and management — re-exports from the engine crate.
//! The engine owns the canonical [`nonoclaw_engine::skills::Skill`] struct,
//! [`SkillsManager`] state machine, and all parsing/discovery logic. The CLI
//! layer re-exports what it needs for command help text and the project info
//! Insight panel.
//!
//! These re-exports exist for external consumers that prefer importing from the
//! CLI crate rather than depending on the engine directly.

#[allow(unused_imports)]
pub use nonoclaw_engine::skills::{Skill, SkillsManager};
