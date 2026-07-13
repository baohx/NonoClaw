//! Agentic query loop, system prompt assembly, and context.
//! Mirrors `src/query.ts`, `src/QueryEngine.ts`, `src/context.ts`.

pub mod compact;
pub mod context;
pub mod hooks;
pub mod loop_;
pub mod prompt;
pub mod session;
pub mod settings;
pub mod skills;
pub mod tokens;

pub use hooks::{lifecycle_context, run_hooks, HookType};
pub use loop_::{
    EngineEvent, EngineOptions, FinalResult, PermissionRequest, PermissionResolver, QueryEngine,
};
pub use session::{
    clear_session, list_sessions, load_session, most_recent_session, new_session_id,
    session_path, SessionInfo,
};
pub use settings::{apply_env, apply_settings, load_settings, ModelProfile, SettingsFile};
pub use skills::{substitute_arguments, Skill, SkillsManager};
