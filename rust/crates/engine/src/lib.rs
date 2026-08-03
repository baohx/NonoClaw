//! Agentic query loop, system prompt assembly, and context.
//! Mirrors `src/query.ts`, `src/QueryEngine.ts`, `src/context.ts`.

pub mod agents;
pub mod compact;
pub mod context;
pub mod extensions;
pub mod graph;
pub mod hooks;
pub mod loop_;
pub mod prompt;
pub mod prompt_templates;
pub mod run;
pub mod session;
pub mod settings;
pub mod skills;
pub mod tokens;
pub mod tool_selector;
pub mod toolchain;
pub mod trace;

pub use hooks::{
    lifecycle_context, load_hooks, load_hooks_with_diagnostics, HookAction, HookDecision, HookDef,
    HookDiagnostic, HookLoadReport, HookRuntime, HookType,
};
pub use loop_::{
    EngineEvent, EngineOptions, FinalResult, PermissionRequest, PermissionResolver, QueryEngine,
    RunFinishReason,
};
pub use nonoclaw_api::{ClientFactory, ClientPurpose};
pub use nonoclaw_core::{
    EventEnvelope, ExtensionDescriptor, ExtensionDiagnostic, ExtensionDiagnosticSeverity,
    ExtensionKind, ExtensionSourceKind, ExtensionStatus, RunEvent, RunId, SessionRepair,
    SessionRepairKind, StreamState, TechnicalStatus,
};
pub use run::{
    RunCompletion, RunContext, RunController, RunHandle, RunLimits, RunTerminal, RunTerminalStatus,
    SequencedEngineEvent,
};
pub use session::{
    new_session_id, session_path, Session, SessionEntry, SessionError, SessionInfo, SessionResult,
    SessionService, SessionSnapshot,
};
pub use settings::{
    config_reference, load_resolved_config, ConfigDiagnostic, ConfigFieldReference, ConfigSource,
    ModelProfile, ResolvedConfig, RunConfigOverrides, SettingsFile,
};
pub use skills::{substitute_arguments, EngineSkillSource, Skill, SkillActivation, SkillsManager};
pub use toolchain::{
    probe_runtime, probe_runtime_with_updates, ExecutableProbe, PythonVenvProbe, RuntimeProbeReport,
};

pub use trace::TraceCollector;

// Tests that temporarily override process-wide config environment variables
// must serialize with one another. Production code does not use this lock.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
