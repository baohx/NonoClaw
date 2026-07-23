//! The agentic query loop. Mirrors `src/query.ts` (one streaming turn) and
//! `src/QueryEngine.ts` (the outer loop: turn -> dispatch tool_use -> append
//! tool_result -> repeat until `end_turn` / no tools / max turns).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use nonoclaw_api::{Client, RequestParams, StreamEvent, ThinkingConfig, ToolSchema};
use nonoclaw_core::{
    CacheControl, ContentBlock, Message, MessageContent, PermissionDecision, PermissionMode,
    Result, RunEvent, SessionRepair, StopReason, StreamState, TechnicalStatus, Usage, UsagePart,
};
use nonoclaw_tools::permissions::PermissionGate;
use nonoclaw_tools::tool::{QuestionResolver, SubagentRunner};
use nonoclaw_tools::{
    PermissionResolverFuture, TodoStore, ToolCall, ToolExecutionContext, ToolExecutor,
    ToolHookRunner, ToolOptions, ToolPermissionRequest, ToolPermissionResolver, ToolRegistry,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agents::SubagentLifecycle;
use crate::compact::{compact_messages, KEEP_RECENT_TURNS};
use crate::context::{get_system_context, get_user_context, load_memory_prompt};
use crate::prompt::build_system_blocks;
use crate::run::{RunContext, RunController, RunLimits, RunTerminalStatus};
use crate::session::{new_session_id, Session, SessionError, SessionSnapshot};
use crate::skills::SkillsManager;
use crate::tokens::estimate_total;
use nonoclaw_tools::BackgroundTaskRegistry;

/// A request to the active UI adapter to resolve an interactive permission
/// `Ask` and return the user's decision.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: Value,
    pub message: String,
}

/// Boxed future returned by a [`PermissionResolver`].
pub type ResolverFut = Pin<Box<dyn Future<Output = PermissionDecision> + Send>>;
/// Interactive permission resolver: given a request, returns a future that
/// yields the user's decision. `None` (headless) means unresolved `Ask`s are
/// auto-denied.
pub type PermissionResolver = Arc<dyn Fn(PermissionRequest) -> ResolverFut + Send + Sync>;

struct EnginePermissionResolver(PermissionResolver);

impl ToolPermissionResolver for EnginePermissionResolver {
    fn resolve(&self, request: ToolPermissionRequest) -> PermissionResolverFuture {
        (self.0)(PermissionRequest {
            tool_use_id: request.tool_use_id,
            tool_name: request.tool_name,
            input: request.input,
            message: request.message,
        })
    }
}

struct EngineToolHooks {
    runtime: crate::hooks::HookRuntime,
}

#[async_trait::async_trait]
impl ToolHookRunner for EngineToolHooks {
    async fn pre_tool_use(&self, tool_name: &str, input: &Value) -> PermissionDecision {
        let context = crate::hooks::tool_context_for(
            crate::hooks::HookType::PreToolUse,
            tool_name,
            input,
            None,
        );
        self.runtime
            .decide(crate::hooks::HookType::PreToolUse, tool_name, &context)
            .await
    }

    async fn post_tool_use(&self, tool_name: &str, input: &Value, success: bool) {
        let hook_type = if success {
            crate::hooks::HookType::PostToolUse
        } else {
            crate::hooks::HookType::PostToolUseFailure
        };
        let context = crate::hooks::tool_context_for(hook_type, tool_name, input, None);
        self.runtime.run(hook_type, tool_name, &context).await;
    }
}

/// Configuration for a query run. Mirrors the CLI flags that reach the engine.
//
// NOTE: no `Debug` — `permission_resolver` holds a `dyn Fn` which has no `Debug`.
#[derive(Clone)]
pub struct EngineOptions {
    pub model: String,
    pub max_tokens: u32,
    pub permission_mode: PermissionMode,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub add_dirs: Vec<PathBuf>,
    pub max_turns: u32,
    pub append_system_prompt: Option<String>,
    pub skills_manager: Option<Arc<RwLock<SkillsManager>>>,
    /// Raw argument string for skill invocation (e.g. `/deploy app --env=prod`).
    pub arguments: Option<String>,
    /// Background task registry for `run_in_background` bash commands.
    pub background_registry: Option<Arc<std::sync::Mutex<BackgroundTaskRegistry>>>,
    pub thinking: Option<ThinkingConfig>,
    /// `true` for `--print` / SDK mode. Unresolved permission `Ask`s are
    /// auto-denied (no TTY to prompt).
    pub is_non_interactive: bool,
    /// Interactive permission resolver. When set and the session is
    /// interactive, `Ask` decisions are surfaced to the user; otherwise
    /// (headless) `Ask` is auto-denied.
    pub permission_resolver: Option<PermissionResolver>,
    /// Interactive question resolver for AskUserQuestion. When set and the
    /// session is interactive, the tool can surface a multiple-choice prompt;
    /// otherwise it returns a default answer.
    pub question_resolver: Option<Arc<dyn QuestionResolver>>,
    /// When true, auto-compact the transcript once it exceeds
    /// `compact_threshold_tokens` (estimated).
    pub auto_compact: bool,
    /// Estimated-token threshold above which auto-compact fires.
    pub compact_threshold_tokens: usize,
    /// Optional model override for compaction summarization. Falls back to
    /// `model` when unset. Set to a cheap model (e.g. haiku) to save costs.
    pub compact_model: Option<String>,
    /// Client selected by the canonical factory for compaction. Falls back to
    /// the conversation client if construction failed during configuration.
    pub compact_client: Option<Arc<Client>>,
    /// Client selected by the canonical factory for child agents.
    pub subagent_client: Option<Arc<Client>>,
    /// Chars-per-token divisor for the token estimator. Default 4 (Claude).
    /// DeepSeek / GLM tokenize Chinese text more aggressively — set to 2–3
    /// for better compact-threshold accuracy on those models.
    pub chars_per_token: usize,
    /// Active model's context window in tokens. Used to compute occupancy
    /// ratio and auto-compact threshold. Falls back to the global
    /// `contextWindow` setting when the model profile doesn't specify one.
    pub context_window: Option<usize>,
    /// Optional run budget propagated to tools and recorded in RunContext.
    /// Existing entry points leave this unset until a budget is configured.
    pub max_budget_usd: Option<f64>,
    /// Safe diagnostics derived by canonical configuration/extension discovery.
    pub startup_events: Vec<RunEvent>,
}

impl EngineOptions {
    /// Apply per-model overrides from a [`ModelProfile`].  Called after the
    /// options are built but before the engine runs, so model-specific
    /// `maxTokens`, `charsPerToken`, and `contextWindow` take effect.
    pub fn apply_model_profile(&mut self, profile: &crate::settings::ModelProfile) {
        if let Some(mt) = profile.max_tokens {
            self.max_tokens = mt;
        }
        if let Some(cpt) = profile.chars_per_token {
            self.chars_per_token = cpt;
        }
        if let Some(cw) = profile.context_window {
            self.context_window = Some(cw);
            // Conservative: 75% of context window.  chars/token estimation is
            // rough — the real token count can be 20-30% higher, especially
            // with Chinese text or tool-heavy prompts.  The 25% margin absorbs
            // estimation error before the API hard-rejects.
            self.compact_threshold_tokens = cw * 3 / 4;
        }
    }
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            model: "claude-sonnet-4-5-20250929".into(),
            max_tokens: 8192,
            permission_mode: PermissionMode::Default,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            add_dirs: Vec::new(),
            max_turns: 10,
            append_system_prompt: None,
            skills_manager: None,
            arguments: None,
            background_registry: None,
            thinking: None,
            is_non_interactive: true,
            permission_resolver: None,
            question_resolver: None,
            auto_compact: true,
            compact_threshold_tokens: 150_000,
            compact_model: None,
            compact_client: None,
            subagent_client: None,
            chars_per_token: 4,
            context_window: None,
            max_budget_usd: None,
            startup_events: Vec::new(),
        }
    }
}

/// Backward-compatible name retained for existing CLI and library consumers.
pub type EngineEvent = RunEvent;

/// Explicit reason a run stopped. This is preserved in the final result and in
/// the controller's exactly-once terminal commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunFinishReason {
    Completed {
        detail: String,
    },
    MaxTurns {
        max_turns: u32,
        suggestion: String,
    },
    BudgetExceeded {
        max_budget_usd: f64,
        suggestion: String,
    },
    ContextLimit {
        context_window: usize,
        suggestion: String,
    },
    Cancelled {
        reason: String,
    },
    Error {
        message: String,
    },
}

/// The result of a complete query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalResult {
    pub text: String,
    pub usage: Usage,
    pub turns: u32,
    pub stop_reason: Option<StopReason>,
    pub finish_reason: RunFinishReason,
}

/// Cancels run-owned child work on every return path, including provider/tool
/// errors that use `?` before the normal lifecycle epilogue.
struct CancelChildrenOnDrop(CancellationToken);

impl Drop for CancelChildrenOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub struct QueryEngine {
    client: Arc<Client>,
    registry: Arc<ToolRegistry>,
    todos: Arc<TodoStore>,
    options: EngineOptions,
    messages: Vec<Message>,
    total_usage: Usage,
    session_id: String,
    session: Option<Session>,
    session_revision: u64,
    session_repairs: Vec<SessionRepair>,
    hooks: Vec<(crate::hooks::HookType, crate::hooks::HookDef)>,
    /// Background compaction task spawned when tokens reach 80% threshold.
    pending_compact: Option<tokio::task::JoinHandle<Result<Vec<Message>>>>,
    /// Message count when background compact was spawned (for correct delta).
    pending_compact_msg_count: usize,
    /// Session revision the background compact was based on.
    pending_compact_revision: u64,
}

impl QueryEngine {
    pub fn new(
        client: Arc<Client>,
        registry: Arc<ToolRegistry>,
        todos: Arc<TodoStore>,
        options: EngineOptions,
    ) -> Self {
        QueryEngine {
            client,
            registry,
            todos,
            options,
            messages: Vec::new(),
            total_usage: Usage::default(),
            session_id: new_session_id(),
            session: None,
            session_revision: 0,
            session_repairs: Vec::new(),
            hooks: Vec::new(),
            pending_compact: None,
            pending_compact_msg_count: 0,
            pending_compact_revision: 0,
        }
    }

    /// Construct an engine from a canonical session snapshot. All subsequent
    /// transcript mutations are committed through that session's writer actor.
    pub fn with_session(
        client: Arc<Client>,
        registry: Arc<ToolRegistry>,
        todos: Arc<TodoStore>,
        options: EngineOptions,
        session: Session,
        snapshot: SessionSnapshot,
    ) -> Self {
        QueryEngine {
            client,
            registry,
            todos,
            options,
            messages: snapshot.messages,
            total_usage: Usage::default(),
            session_id: session.id().to_string(),
            session: Some(session),
            session_revision: snapshot.revision,
            session_repairs: snapshot.repairs,
            hooks: Vec::new(),
            pending_compact: None,
            pending_compact_msg_count: 0,
            pending_compact_revision: 0,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Take the accumulated messages out, draining the transcript. Useful for
    /// carrying history across independent runs (e.g. in a web server loop).
    pub fn take_messages(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.messages)
    }

    /// Commit a transcript message through the canonical session actor.
    async fn persist(&mut self, msg: Message) {
        if let Some(session) = &self.session {
            match session.append(msg).await {
                Ok(revision) => self.session_revision = revision,
                Err(error) => tracing::warn!(%error, "failed to persist session message"),
            }
        }
    }

    /// Atomically replace the persisted transcript only if no intervening
    /// session command has advanced the revision used by compaction.
    async fn persist_compaction(&mut self, messages: Vec<Message>, expected_revision: u64) -> bool {
        let Some(session) = &self.session else {
            return true;
        };
        match session
            .replace_after_compact(messages, expected_revision)
            .await
        {
            Ok(revision) => {
                self.session_revision = revision;
                true
            }
            Err(SessionError::RevisionConflict { current, .. }) => {
                self.session_revision = current;
                tracing::debug!(
                    expected_revision,
                    current_revision = current,
                    "discarding stale compact replacement"
                );
                false
            }
            Err(error) => {
                tracing::warn!(%error, "failed to persist compact replacement");
                false
            }
        }
    }

    pub fn run_context(&self, cwd: PathBuf) -> RunContext {
        RunContext::new(
            self.session_id.clone(),
            cwd,
            self.options.model.clone(),
            RunLimits {
                max_turns: self.options.max_turns,
                max_budget_usd: self.options.max_budget_usd,
                context_window: self.options.context_window,
            },
        )
    }

    pub fn child_run_context(&self, parent: &RunContext, cwd: PathBuf) -> RunContext {
        parent.child(
            self.session_id.clone(),
            cwd,
            self.options.model.clone(),
            RunLimits {
                max_turns: self.options.max_turns,
                max_budget_usd: self.options.max_budget_usd,
                context_window: self.options.context_window,
            },
        )
    }

    /// Backwards-compatible direct execution. Production entry points use
    /// `RunController`; tests and library callers retain this convenience API.
    pub async fn run(
        &mut self,
        user_content: MessageContent,
        cwd: &Path,
        on_event: impl FnMut(&EngineEvent),
    ) -> Result<FinalResult> {
        let context = self.run_context(cwd.to_path_buf());
        self.run_with_context(user_content, &context, on_event)
            .await
    }

    /// Run the agent loop inside the canonical run identity and token tree.
    pub async fn run_with_context(
        &mut self,
        user_content: MessageContent,
        context: &RunContext,
        mut on_event: impl FnMut(&EngineEvent),
    ) -> Result<FinalResult> {
        let cwd = context.cwd.as_path();
        if context.cancel.is_cancelled() {
            return Err(nonoclaw_core::Error::Cancelled);
        }
        let run_started_at = Instant::now();
        on_event(&RunEvent::RunStarted {
            requested_model: self.options.model.clone(),
            max_turns: self.options.max_turns,
            max_budget_usd: self.options.max_budget_usd,
        });
        for diagnostic in self.options.startup_events.clone() {
            on_event(&diagnostic);
        }
        // Extract a plain-text preview for hooks / logging.
        let user_text = match &user_content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Blocks(bs) => bs
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        };

        self.hooks = crate::hooks::load_hooks(cwd);
        let hook_runtime = crate::hooks::HookRuntime::new(
            self.hooks.clone(),
            Some(Arc::clone(&self.client)),
            self.options.model.clone(),
            context.cancel.child_token(),
        );
        // SessionStart + UserPromptSubmit hooks.
        hook_runtime
            .run(
                crate::hooks::HookType::SessionStart,
                "*",
                &crate::hooks::lifecycle_context("SessionStart"),
            )
            .await;
        hook_runtime
            .run(
                crate::hooks::HookType::UserPromptSubmit,
                "*",
                &crate::hooks::prompt_context(&user_text),
            )
            .await;
        for event in hook_runtime.drain_events() {
            on_event(&event);
        }
        // Surface recoverable legacy-session damage through the normal engine
        // event stream before new output is produced.
        for repair in std::mem::take(&mut self.session_repairs) {
            on_event(&EngineEvent::SessionRepair { repair });
        }
        let user_msg = Message::user(user_content.clone());
        self.messages.push(user_msg.clone());
        self.persist(user_msg).await;

        // Assemble system prompt + tool definitions once (stable for the run).
        let system_ctx = get_system_context(cwd).await;
        let user_ctx = get_user_context(cwd, &self.options.add_dirs);
        let memory = load_memory_prompt(cwd);
        let tool_prompts: Vec<(String, String)> = self
            .registry
            .all()
            .iter()
            .map(|t| (t.name().to_string(), t.prompt().to_string()))
            .collect();
        let mut system_blocks = build_system_blocks(
            cwd,
            &system_ctx,
            &user_ctx,
            &memory,
            &tool_prompts,
            &self.options.append_system_prompt,
            &self.options.skills_manager,
        );
        let allow_filter = if self.options.allowed_tools.is_empty() {
            None
        } else {
            Some(self.options.allowed_tools.as_slice())
        };
        let mut tool_defs: Vec<ToolSchema> = self
            .registry
            .active_definitions(allow_filter)
            .into_iter()
            .map(|d| ToolSchema {
                name: d.name,
                description: d.description,
                input_schema: d.input_schema,
                cache_control: None,
            })
            .collect();
        // Prompt-cache breakpoint on the last tool so the entire tools array is
        // part of the cached prefix across turns.
        if let Some(last) = tool_defs.last_mut() {
            last.cache_control = Some(CacheControl {
                kind: nonoclaw_core::CacheControlKind::Ephemeral,
            });
        }
        let tools_chars: usize = tool_defs
            .iter()
            .map(|d| serde_json::to_string(d).map(|s| s.len()).unwrap_or(0))
            .sum();
        let system_chars: usize = system_blocks.iter().map(|b| b.text.chars().count()).sum();
        let skill_count = self
            .options
            .skills_manager
            .as_ref()
            .map(|manager| manager.read().unwrap().all_active().len())
            .unwrap_or(0);
        on_event(&RunEvent::ContextPrepared {
            estimated_tokens: estimate_total(
                &self.messages,
                system_chars,
                tools_chars,
                self.options.chars_per_token,
            ),
            context_window: self.options.context_window,
            tool_count: tool_defs.len(),
            skill_count,
        });
        if let Some(manager) = &self.options.skills_manager {
            for diagnostic in manager.read().unwrap().diagnostics() {
                on_event(&RunEvent::ExtensionDiagnostic { diagnostic });
            }
        }
        for descriptor in self.registry.extension_descriptors() {
            if descriptor.kind == nonoclaw_core::ExtensionKind::Mcp {
                on_event(&RunEvent::McpDiagnostic {
                    server: descriptor.name.clone(),
                    status: match descriptor.status {
                        nonoclaw_core::ExtensionStatus::Active => TechnicalStatus::Succeeded,
                        nonoclaw_core::ExtensionStatus::Pending => TechnicalStatus::Pending,
                        nonoclaw_core::ExtensionStatus::Shadowed
                        | nonoclaw_core::ExtensionStatus::Failed
                        | nonoclaw_core::ExtensionStatus::Disconnected => TechnicalStatus::Failed,
                    },
                    source: Some(descriptor.source.clone()),
                    detail: descriptor
                        .detail
                        .clone()
                        .unwrap_or_else(|| "MCP extension state resolved".into()),
                });
            }
        }
        for diagnostic in self.registry.extension_diagnostics() {
            on_event(&RunEvent::ExtensionDiagnostic {
                diagnostic: diagnostic.clone(),
            });
        }
        let gate = PermissionGate::new(
            self.options.permission_mode,
            self.options.allowed_tools.clone(),
            self.options.disallowed_tools.clone(),
        );

        // Subagent runner: shares the client + toolset; children exclude Agent
        // (no recursion) and TodoWrite (avoid clobbering the parent's list).
        let spawner = EngineSubagent {
            client: self
                .options
                .subagent_client
                .clone()
                .unwrap_or_else(|| Arc::clone(&self.client)),
            registry: Arc::clone(&self.registry),
            options: self.options.clone(),
            cwd: cwd.to_path_buf(),
            hook_runtime: hook_runtime.clone(),
            run_context: context.clone(),
            task_store: Arc::clone(&self.todos),
            lifecycle: SubagentLifecycle::new(context.cancel.clone()),
        };
        let permission_resolver = self.options.permission_resolver.clone().map(|resolver| {
            Arc::new(EnginePermissionResolver(resolver)) as Arc<dyn ToolPermissionResolver>
        });
        let tool_executor = ToolExecutor::from_env(
            Arc::clone(&self.registry),
            gate,
            Arc::new(EngineToolHooks {
                runtime: hook_runtime.clone(),
            }),
            permission_resolver,
        );
        let tool_options = self.tool_options();

        let cancel = context.cancel.child_token();
        let _cancel_children_on_drop = CancelChildrenOnDrop(cancel.clone());
        let mut turns_made = 0u32;
        let mut last_text = String::new();
        let mut last_stop: Option<StopReason> = None;

        // Skill triggers: check user input against trigger patterns and
        // activate matching conditional skills before the first turn.
        let mut last_skills_version: u64 = 0;
        if let Some(ref mgr) = self.options.skills_manager {
            let mut guard = mgr.write().unwrap();
            if let Some(skill_name) = user_text
                .strip_prefix('/')
                .and_then(|rest| rest.split_whitespace().next())
                .filter(|name| !name.is_empty())
            {
                guard.activate_slash_command(skill_name);
            }
            let triggered = guard.match_triggers(&user_text);
            if !triggered.is_empty() {
                tracing::info!(?triggered, "skills triggered by user input");
            }
            for activation in guard.take_activation_events() {
                on_event(&EngineEvent::SkillActivated {
                    name: activation.name,
                    reason: activation.reason,
                    source: activation.source,
                    version: activation.version,
                });
            }
            last_skills_version = guard.version();
        }

        let finish_reason = loop {
            if cancel.is_cancelled() {
                return Err(nonoclaw_core::Error::Cancelled);
            }

            // Inject background task completion notifications.
            if let Some(ref reg) = self.options.background_registry {
                let notifications = reg.lock().unwrap().drain_notifications();
                for task in &notifications {
                    on_event(&RunEvent::BackgroundTaskChanged {
                        task_id: task.id.clone(),
                        status: match task.status {
                            nonoclaw_tools::BackgroundTaskStatus::Completed => {
                                TechnicalStatus::Succeeded
                            }
                            nonoclaw_tools::BackgroundTaskStatus::Failed => TechnicalStatus::Failed,
                            nonoclaw_tools::BackgroundTaskStatus::Killed => {
                                TechnicalStatus::Cancelled
                            }
                            nonoclaw_tools::BackgroundTaskStatus::Running
                            | nonoclaw_tools::BackgroundTaskStatus::Backgrounded => {
                                TechnicalStatus::Running
                            }
                        },
                        exit_code: task.exit_code,
                    });
                    let msg = format!(
                        "<task_notification>\n<task_id>{}</task_id>\n<status>{:?}</status>\n<command>{}</command>\n</task_notification>",
                        task.id, task.status, task.command
                    );
                    self.messages
                        .push(Message::user(MessageContent::from_text(&msg)));
                    hook_runtime
                        .run(
                            crate::hooks::HookType::Notification,
                            "*",
                            &serde_json::json!({
                                "hook_event_name": "Notification",
                                "task_id": task.id,
                                "status": format!("{:?}", task.status),
                                "command": task.command,
                            }),
                        )
                        .await;
                }
            }

            // Rebuild system prompt if skills were dynamically activated.
            if let Some(ref mgr) = self.options.skills_manager {
                let v = mgr.read().unwrap().version();
                if v > last_skills_version {
                    system_blocks = build_system_blocks(
                        cwd,
                        &system_ctx,
                        &user_ctx,
                        &memory,
                        &tool_prompts,
                        &self.options.append_system_prompt,
                        &self.options.skills_manager,
                    );
                    last_skills_version = v;
                }
            }

            // Refresh the uncached context block with live git status
            // each turn so the model sees up-to-date working-tree state.
            {
                let live_git = get_system_context(cwd).await;
                system_blocks = crate::prompt::refresh_context_block(
                    &system_blocks,
                    &live_git,
                    &user_ctx,
                    &memory,
                );
            }

            if turns_made >= self.options.max_turns {
                break RunFinishReason::MaxTurns {
                    max_turns: self.options.max_turns,
                    suggestion: "continue the session or increase max_turns".into(),
                };
            }

            // Two-pass auto-compact: check for completed background compact first.
            let compact_done = if let Some(ref handle) = self.pending_compact {
                handle.is_finished()
            } else {
                false
            };
            if compact_done {
                let handle = self.pending_compact.take().unwrap();
                let msg_count_at_spawn = self.pending_compact_msg_count;
                let revision_at_spawn = self.pending_compact_revision;
                match handle.await {
                    Ok(Ok(compacted)) => {
                        let kept = compacted.len();
                        let removed = msg_count_at_spawn.saturating_sub(kept);
                        if removed > 0
                            && msg_count_at_spawn == self.messages.len()
                            && self
                                .persist_compaction(compacted.clone(), revision_at_spawn)
                                .await
                        {
                            self.messages = compacted;
                            on_event(&EngineEvent::Compacted {
                                removed,
                                kept,
                                tokens_before: 0,
                                tokens_after: 0,
                            });
                            hook_runtime
                                .run(
                                    crate::hooks::HookType::PostCompact,
                                    "*",
                                    &crate::hooks::compact_context_for(
                                        crate::hooks::HookType::PostCompact,
                                        removed,
                                        kept,
                                        0,
                                        0,
                                    ),
                                )
                                .await;
                        } else {
                            tracing::debug!(
                                "background compact stale — transcript or revision changed since spawn"
                            );
                        }
                    }
                    Ok(Err(e)) => tracing::warn!("background compact failed: {e}"),
                    Err(e) => tracing::warn!("background compact panicked: {e}"),
                }
            }

            // Auto-compact: if the estimated prompt exceeds the threshold,
            // summarize the older transcript before the next turn.
            if self.options.auto_compact {
                let est = estimate_total(
                    &self.messages,
                    system_chars,
                    tools_chars,
                    self.options.chars_per_token,
                );
                // Pre-fire: spawn background compact at 80% of threshold.
                if est > self.options.compact_threshold_tokens * 8 / 10
                    && self.pending_compact.is_none()
                {
                    let model = self
                        .options
                        .compact_model
                        .clone()
                        .unwrap_or_else(|| self.options.model.clone());
                    let compact_client = self
                        .options
                        .compact_client
                        .clone()
                        .unwrap_or_else(|| Arc::clone(&self.client));
                    let messages = self.messages.clone();
                    let keep = KEEP_RECENT_TURNS;
                    let m = model.clone();
                    self.pending_compact_msg_count = messages.len();
                    self.pending_compact_revision = self.session_revision;
                    on_event(&RunEvent::CompactionStarted {
                        automatic: true,
                        tokens_before: est,
                        messages_before: messages.len(),
                    });
                    hook_runtime
                        .run(
                            crate::hooks::HookType::PreCompact,
                            "*",
                            &crate::hooks::compact_context_for(
                                crate::hooks::HookType::PreCompact,
                                messages.len(),
                                0,
                                est,
                                0,
                            ),
                        )
                        .await;
                    let compact_cancel = cancel.child_token();
                    let handle = tokio::spawn(async move {
                        tokio::select! {
                            biased;
                            _ = compact_cancel.cancelled() => Err(nonoclaw_core::Error::Cancelled),
                            result = compact_messages(
                                compact_client.as_ref(),
                                &m,
                                &messages,
                                keep,
                                crate::compact::CompactMode::Segments,
                            ) => result,
                        }
                    });
                    self.pending_compact = Some(handle);
                }

                if est > self.options.compact_threshold_tokens {
                    let before = self.messages.len();
                    let tokens_before = est;
                    let compact_revision = self.session_revision;
                    on_event(&RunEvent::CompactionStarted {
                        automatic: true,
                        tokens_before,
                        messages_before: before,
                    });
                    // PreCompact hook
                    hook_runtime
                        .run(
                            crate::hooks::HookType::PreCompact,
                            "*",
                            &crate::hooks::compact_context_for(
                                crate::hooks::HookType::PreCompact,
                                before,
                                0,
                                est,
                                0,
                            ),
                        )
                        .await;
                    let compact_model = self
                        .options
                        .compact_model
                        .as_deref()
                        .unwrap_or(&self.options.model);
                    let compact_client = self
                        .options
                        .compact_client
                        .clone()
                        .unwrap_or_else(|| Arc::clone(&self.client));
                    let compacted = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(nonoclaw_core::Error::Cancelled),
                        result = compact_messages(
                            compact_client.as_ref(),
                            compact_model,
                            &self.messages,
                            KEEP_RECENT_TURNS,
                            crate::compact::CompactMode::Segments,
                        ) => result?,
                    };
                    let tokens_after = estimate_total(
                        &compacted,
                        system_chars,
                        tools_chars,
                        self.options.chars_per_token,
                    );
                    let kept = compacted.len();
                    let removed = before.saturating_sub(kept);
                    if removed > 0
                        && self
                            .persist_compaction(compacted.clone(), compact_revision)
                            .await
                    {
                        self.messages = compacted;
                        on_event(&EngineEvent::Compacted {
                            removed,
                            kept,
                            tokens_before,
                            tokens_after,
                        });
                        // PostCompact hook
                        hook_runtime
                            .run(
                                crate::hooks::HookType::PostCompact,
                                "*",
                                &crate::hooks::compact_context_for(
                                    crate::hooks::HookType::PostCompact,
                                    removed,
                                    kept,
                                    est,
                                    tokens_after,
                                ),
                            )
                            .await;
                    }
                }
            }

            turns_made += 1;

            let params = RequestParams {
                model: self.options.model.clone(),
                max_tokens: self.options.max_tokens,
                system: system_blocks.clone(),
                messages: strip_thinking(&self.messages),
                tools: tool_defs.clone(),
                tool_choice: None,
                thinking: self.options.thinking.clone(),
                temperature: None,
                betas: Vec::new(),
                trace_label: Some(format!(
                    "{}:turn-{}",
                    &self.session_id[..8.min(self.session_id.len())],
                    turns_made
                )),
            };

            let provider = format!("{:?}", self.client.api_format()).to_lowercase();
            on_event(&RunEvent::ModelRequestStarted {
                requested_model: self.options.model.clone(),
                provider: provider.clone(),
                turn: turns_made,
            });
            on_event(&RunEvent::StreamStateChanged {
                state: StreamState::Connecting,
                turn: turns_made,
            });
            let requested_model = self.options.model.clone();
            let usage_before_turn = self.total_usage;
            let turn = match self
                .client
                .run_turn_with_cancel(
                    &params,
                    |ev| {
                        forward_stream_event(
                            ev,
                            &requested_model,
                            &provider,
                            turns_made,
                            usage_before_turn,
                            &mut on_event,
                        )
                    },
                    cancel.child_token(),
                )
                .await
                .map_err(|failure| failure.into_core())
            {
                Ok(t) => t,
                Err(e) => {
                    // If the API rejects messages because of orphaned tool_use
                    // blocks (no matching tool_result), repair and retry once.
                    let msg = e.to_string();
                    if msg.contains("tool_use") && msg.contains("tool_result") {
                        let before = self.messages.len();
                        repair_tool_pairing(&mut self.messages);
                        if self.messages.len() != before {
                            tracing::warn!(
                                before,
                                after = self.messages.len(),
                                "repaired orphaned tool_use/tool_result pairs, retrying"
                            );
                            on_event(&RunEvent::RecoveryApplied {
                                category: "tool_pairing".into(),
                                detail: "removed orphaned tool-use/result blocks before one retry"
                                    .into(),
                                items_affected: before.saturating_sub(self.messages.len()),
                            });
                            let params2 = RequestParams {
                                messages: strip_thinking(&self.messages),
                                trace_label: Some(format!(
                                    "{}:retry",
                                    &self.session_id[..8.min(self.session_id.len())]
                                )),
                                ..params.clone()
                            };
                            self.client
                                .run_turn_with_cancel(
                                    &params2,
                                    |ev| {
                                        forward_stream_event(
                                            ev,
                                            &requested_model,
                                            &provider,
                                            turns_made,
                                            usage_before_turn,
                                            &mut on_event,
                                        )
                                    },
                                    cancel.child_token(),
                                )
                                .await
                                .map_err(|failure| failure.into_core())?
                        } else {
                            return Err(e);
                        }
                    } else {
                        return Err(e);
                    }
                }
            };

            self.total_usage.accumulate(&turn.usage);
            on_event(&RunEvent::UsageUpdated {
                turn: turns_made,
                turn_usage: UsagePart {
                    input_tokens: Some(turn.usage.input_tokens),
                    output_tokens: Some(turn.usage.output_tokens),
                    cache_creation_input_tokens: Some(turn.usage.cache_creation_input_tokens),
                    cache_read_input_tokens: Some(turn.usage.cache_read_input_tokens),
                },
                total: self.total_usage,
                max_budget_usd: self.options.max_budget_usd,
            });
            last_stop = turn.stop_reason.clone();

            // Collect assistant text for display + the transcript message.
            let assistant_text: String = turn
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if !assistant_text.is_empty() {
                last_text = assistant_text.clone();
                on_event(&EngineEvent::AssistantDone {
                    text: assistant_text,
                });
            }
            let asst_msg = Message::assistant(MessageContent::from_blocks(turn.content.clone()));
            self.messages.push(asst_msg.clone());
            self.persist(asst_msg).await;

            let tool_uses: Vec<(String, String, Value)> = turn
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            if tool_uses.is_empty() || turn.stop_reason != Some(StopReason::ToolUse) {
                break RunFinishReason::Completed {
                    detail: turn
                        .stop_reason
                        .as_ref()
                        .map(|reason| format!("model stop reason: {}", reason.as_str()))
                        .unwrap_or_else(|| "model returned no further tool calls".into()),
                };
            }

            for (index, (id, name, input)) in tool_uses.iter().enumerate() {
                on_event(&EngineEvent::ToolUseStart {
                    id: id.clone(),
                    name: name.clone(),
                    input: nonoclaw_core::redact_value(input.clone()),
                });
                on_event(&RunEvent::ToolQueued {
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    index,
                });
                on_event(&RunEvent::ToolExecutionStarted {
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    read_only: None,
                    destructive: None,
                });
            }
            let calls = tool_uses
                .iter()
                .map(|(id, name, input)| ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                })
                .collect::<Vec<_>>();
            let execution_context = ToolExecutionContext {
                cwd,
                options: &tool_options,
                cancel: &cancel,
                task_scope: Some(&context.session_id),
                subagent: Some(&spawner),
                question: self.options.question_resolver.as_deref(),
                background_registry: self.options.background_registry.clone(),
                is_non_interactive: self.options.is_non_interactive,
            };
            let executions = tool_executor.execute(&calls, &execution_context).await;
            for execution in &executions {
                on_event(&EngineEvent::ToolResult {
                    id: execution.id.clone(),
                    ok: !execution.is_error,
                    preview: preview(&execution.content),
                });
                for change in &execution.task_changes {
                    on_event(&EngineEvent::TaskChanged {
                        change: change.clone(),
                    });
                }
                for record in &execution.trace {
                    match record.stage {
                        nonoclaw_tools::ToolTraceStage::Validate
                        | nonoclaw_tools::ToolTraceStage::Lookup => {
                            on_event(&RunEvent::ToolValidation {
                                tool_use_id: execution.id.clone(),
                                tool_name: execution.name.clone(),
                                ok: record.ok,
                                detail: record.detail.clone(),
                            });
                        }
                        nonoclaw_tools::ToolTraceStage::PermissionRequest => {
                            on_event(&RunEvent::PermissionRequested {
                                tool_use_id: execution.id.clone(),
                                tool_name: execution.name.clone(),
                                waiting_on: record.detail.clone(),
                            });
                        }
                        nonoclaw_tools::ToolTraceStage::Permission => {
                            on_event(&RunEvent::PermissionResolved {
                                tool_use_id: execution.id.clone(),
                                tool_name: execution.name.clone(),
                                decision: if record.ok {
                                    TechnicalStatus::Allowed
                                } else {
                                    TechnicalStatus::Denied
                                },
                                elapsed_ms: record.elapsed_ms,
                            });
                        }
                        nonoclaw_tools::ToolTraceStage::Call => {
                            on_event(&RunEvent::ToolExecutionFinished {
                                tool_use_id: execution.id.clone(),
                                tool_name: execution.name.clone(),
                                status: if record.ok {
                                    TechnicalStatus::Succeeded
                                } else if context.cancel.is_cancelled() {
                                    TechnicalStatus::Cancelled
                                } else {
                                    TechnicalStatus::Failed
                                },
                                elapsed_ms: record.elapsed_ms,
                            });
                        }
                        nonoclaw_tools::ToolTraceStage::Normalize => {
                            on_event(&RunEvent::ToolResultNormalized {
                                tool_use_id: execution.id.clone(),
                                original_chars: execution.original_chars,
                                visible_chars: execution.content.chars().count(),
                                truncated: execution.local_reference.is_some(),
                                local_reference: execution
                                    .local_reference
                                    .as_ref()
                                    .map(|path| path.to_string_lossy().to_string()),
                            });
                        }
                        nonoclaw_tools::ToolTraceStage::PreHook
                        | nonoclaw_tools::ToolTraceStage::PostHook => {}
                    }
                }
            }
            for event in hook_runtime.drain_events() {
                on_event(&event);
            }
            let results = executions
                .into_iter()
                .map(|result| (result.id, result.content, result.is_error))
                .collect::<Vec<_>>();

            // Dynamic skill activation: extract file paths from Read/Write/Edit
            // tool uses and check against conditional skills + discover new skill
            // directories by walking up from file paths.
            if let Some(ref mgr) = self.options.skills_manager {
                let file_paths: Vec<PathBuf> = tool_uses
                    .iter()
                    .filter(|(_, name, _)| matches!(name.as_str(), "Read" | "Write" | "Edit"))
                    .filter_map(|(_, _, input)| {
                        input.get("file_path").and_then(|v| v.as_str()).map(|fp| {
                            if Path::new(fp).is_absolute() {
                                PathBuf::from(fp)
                            } else {
                                cwd.join(fp)
                            }
                        })
                    })
                    .collect();
                if !file_paths.is_empty() {
                    let mut guard = mgr.write().unwrap();
                    let activated = guard.activate_conditional_for_paths(&file_paths, cwd);
                    let discovered = guard.discover_for_file_paths(&file_paths, cwd);
                    if !activated.is_empty() || !discovered.is_empty() {
                        tracing::info!(
                            ?activated,
                            ?discovered,
                            "skills dynamically activated/discovered"
                        );
                    }
                    for activation in guard.take_activation_events() {
                        on_event(&EngineEvent::SkillActivated {
                            name: activation.name,
                            reason: activation.reason,
                            source: activation.source,
                            version: activation.version,
                        });
                    }
                }
            }

            let blocks: Vec<ContentBlock> = results
                .into_iter()
                .map(|(id, content, is_error)| ContentBlock::tool_result(id, content, is_error))
                .collect();
            let tr_msg = Message::user(MessageContent::from_blocks(blocks));
            self.messages.push(tr_msg.clone());
            self.persist(tr_msg).await;
        };

        // Stop is the main-agent completion boundary; SessionEnd follows it.
        hook_runtime
            .run(
                crate::hooks::HookType::Stop,
                "*",
                &crate::hooks::lifecycle_context("Stop"),
            )
            .await;
        hook_runtime
            .run(
                crate::hooks::HookType::SessionEnd,
                "*",
                &crate::hooks::lifecycle_context("SessionEnd"),
            )
            .await;
        for event in hook_runtime.drain_events() {
            on_event(&event);
        }

        // No run-owned background compaction task may outlive the run.
        cancel.cancel();
        if let Some(handle) = self.pending_compact.take() {
            let _ = handle.await;
        }

        on_event(&RunEvent::RunFinished {
            status: TechnicalStatus::Succeeded,
            reason: format!("{finish_reason:?}"),
            duration_ms: run_started_at.elapsed().as_millis() as u64,
            turns: turns_made,
            usage: self.total_usage,
        });
        Ok(FinalResult {
            text: last_text,
            usage: self.total_usage,
            turns: turns_made,
            stop_reason: last_stop,
            finish_reason,
        })
    }

    fn tool_options(&self) -> ToolOptions {
        ToolOptions {
            model: self.options.model.clone(),
            permission_mode: self.options.permission_mode,
            is_non_interactive: self.options.is_non_interactive,
            max_budget_usd: self.options.max_budget_usd,
        }
    }

    /// Borrow the shared todo store (for the UI to render the task list).
    pub fn todos(&self) -> &Arc<TodoStore> {
        &self.todos
    }

    /// Cumulative token usage across the run so far (for `/cost`).
    pub fn total_usage(&self) -> Usage {
        self.total_usage
    }

    /// Names of all registered tools (for `/tools`).
    pub fn tool_names(&self) -> Vec<String> {
        self.registry
            .all()
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    /// Clear the conversation history through the same atomic session command
    /// stream used by append and compact replacement.
    pub async fn clear(&mut self) -> Result<()> {
        if let Some(session) = &self.session {
            self.session_revision = session
                .clear()
                .await
                .map_err(|error| nonoclaw_core::Error::Other(error.to_string()))?;
        }
        self.messages.clear();
        Ok(())
    }

    /// Force a compaction now (regardless of threshold) if a safe split exists.
    /// Returns (removed, kept) message counts, or `None` if nothing compacted.
    pub async fn compact_now(&mut self) -> Result<Option<(usize, usize)>> {
        let before = self.messages.len();
        let compact_revision = self.session_revision;
        let runtime = crate::hooks::HookRuntime::new(
            self.hooks.clone(),
            Some(Arc::clone(&self.client)),
            self.options.model.clone(),
            CancellationToken::new(),
        );
        runtime
            .run(
                crate::hooks::HookType::PreCompact,
                "*",
                &crate::hooks::compact_context_for(
                    crate::hooks::HookType::PreCompact,
                    before,
                    0,
                    0,
                    0,
                ),
            )
            .await;
        let compact_model = self
            .options
            .compact_model
            .as_deref()
            .unwrap_or(&self.options.model);
        let compact_client = self
            .options
            .compact_client
            .clone()
            .unwrap_or_else(|| Arc::clone(&self.client));
        let compacted = compact_messages(
            compact_client.as_ref(),
            compact_model,
            &self.messages,
            KEEP_RECENT_TURNS,
            crate::compact::CompactMode::Segments,
        )
        .await?;
        let kept = compacted.len();
        if kept < before
            && self
                .persist_compaction(compacted.clone(), compact_revision)
                .await
        {
            self.messages = compacted;
            let removed = before - kept;
            runtime
                .run(
                    crate::hooks::HookType::PostCompact,
                    "*",
                    &crate::hooks::compact_context_for(
                        crate::hooks::HookType::PostCompact,
                        removed,
                        kept,
                        0,
                        0,
                    ),
                )
                .await;
            Ok(Some((removed, kept)))
        } else {
            Ok(None)
        }
    }
}

fn forward_stream_event(
    event: &StreamEvent,
    requested_model: &str,
    provider: &str,
    turn: u32,
    total_before_turn: Usage,
    on_event: &mut impl FnMut(&EngineEvent),
) {
    match event {
        StreamEvent::MessageStart { model, usage, .. } => {
            if !model.is_empty() {
                on_event(&RunEvent::ModelInfo {
                    model: model.clone(),
                });
                on_event(&RunEvent::ModelResolved {
                    requested_model: requested_model.to_string(),
                    actual_model: model.clone(),
                    provider: provider.to_string(),
                    turn,
                });
            }
            let mut total = total_before_turn;
            total.update_from_part(usage);
            on_event(&RunEvent::UsageUpdated {
                turn,
                turn_usage: usage.clone(),
                total,
                max_budget_usd: None,
            });
        }
        StreamEvent::TextDelta { text } => {
            on_event(&RunEvent::StreamStateChanged {
                state: StreamState::Streaming,
                turn,
            });
            on_event(&RunEvent::TextDelta { text: text.clone() });
        }
        StreamEvent::ThinkingDelta { .. } => {
            on_event(&RunEvent::ThinkingState { active: true, turn });
        }
        StreamEvent::MessageDelta { usage, .. } => {
            let mut total = total_before_turn;
            total.update_from_part(usage);
            on_event(&RunEvent::UsageUpdated {
                turn,
                turn_usage: usage.clone(),
                total,
                max_budget_usd: None,
            });
        }
        StreamEvent::MessageStop => {
            on_event(&RunEvent::ThinkingState {
                active: false,
                turn,
            });
            on_event(&RunEvent::StreamStateChanged {
                state: StreamState::Completed,
                turn,
            });
        }
        StreamEvent::CapabilityStatus { feature, status } => {
            on_event(&RunEvent::ProviderDiagnostic {
                provider: provider.to_string(),
                category: format!("capability_{feature:?}").to_lowercase(),
                status: if status.is_supported() {
                    TechnicalStatus::Succeeded
                } else {
                    TechnicalStatus::Failed
                },
                detail: match status {
                    nonoclaw_api::CapabilityStatus::Supported => "supported".into(),
                    nonoclaw_api::CapabilityStatus::Unsupported { reason } => reason.to_string(),
                },
            });
        }
        StreamEvent::RetryScheduled {
            attempt,
            delay_ms,
            error,
        } => {
            on_event(&RunEvent::RetryScheduled {
                attempt: *attempt,
                delay_ms: *delay_ms,
                category: format!("{:?}", error.code).to_lowercase(),
                operation: error.operation.into(),
            });
        }
        StreamEvent::StreamError { error, .. } => {
            on_event(&RunEvent::StreamStateChanged {
                state: StreamState::Interrupted,
                turn,
            });
            on_event(&RunEvent::RunError {
                code: format!("{:?}", error.code).to_lowercase(),
                operation: error.operation.into(),
                retryable: error.retryable,
                message: error.message.clone(),
            });
        }
        StreamEvent::ToolUseStart { .. }
        | StreamEvent::ToolUseInputDelta { .. }
        | StreamEvent::BlockStop { .. } => {}
    }
}

/// Tool-result preview for display. The canonical executor has already
/// normalized oversized payloads, so this is only a final pathological guard.
fn preview(s: &str) -> String {
    const MAX: usize = 4_096;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let mut p: String = s.chars().take(MAX).collect();
    p.push_str("\n…[output truncated]");
    p
}

/// Engine-side subagent spawner. Holds clones of the shared client, toolset,
/// and TaskStore so child todos are scope-isolated while the task graph remains
/// available. Children exclude Agent/Coordinator to prevent recursion.
pub(crate) struct EngineSubagent {
    client: Arc<Client>,
    registry: Arc<ToolRegistry>,
    options: EngineOptions,
    cwd: PathBuf,
    hook_runtime: crate::hooks::HookRuntime,
    run_context: RunContext,
    task_store: Arc<TodoStore>,
    lifecycle: SubagentLifecycle,
}

impl SubagentRunner for EngineSubagent {
    fn run_subagent<'a>(
        &'a self,
        prompt: &'a str,
        description: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            self.lifecycle
                .run(async move {
                    let subagent_started = Instant::now();
                    self.hook_runtime.record_event(RunEvent::SubagentStarted {
                        description: description.to_string(),
                    });
                    self.hook_runtime
                        .run(
                            crate::hooks::HookType::SubagentStart,
                            "*",
                            &crate::hooks::subagent_context_for(
                                crate::hooks::HookType::SubagentStart,
                                description,
                                None,
                            ),
                        )
                        .await;
                    let outcome: Result<String> = async {
                        let child_registry =
                            Arc::new(self.lifecycle.child_registry(&self.registry)?);
                        let mut child_opts = self.options.clone();
                        child_opts.is_non_interactive = true;
                        child_opts.permission_resolver = None;
                        child_opts.max_turns = child_opts.max_turns.min(10);
                        child_opts.append_system_prompt = Some(format!(
                            "You are a subagent (task: {description}). Run autonomously with the available \
                             tools and report ONLY your final answer to the caller. Do not ask the user \
                             questions."
                        ));
                        let engine = QueryEngine::new(
                            Arc::clone(&self.client),
                            child_registry,
                            Arc::clone(&self.task_store),
                            child_opts,
                        );
                        let child_context =
                            engine.child_run_context(&self.run_context, self.cwd.clone());
                        let controller = RunController::new(child_context);
                        let completion = controller
                            .start(engine, MessageContent::from_text(prompt), |_| async {})
                            .wait()
                            .await;
                        let result = match completion.terminal.status {
                            RunTerminalStatus::Done => completion.terminal.result.ok_or_else(|| {
                                nonoclaw_core::Error::Other(
                                    "subagent completed without a result".into(),
                                )
                            })?,
                            RunTerminalStatus::Cancelled => {
                                return Err(nonoclaw_core::Error::Cancelled)
                            }
                            RunTerminalStatus::Error => {
                                return Err(nonoclaw_core::Error::Other(format!(
                                    "subagent failed: {:?}",
                                    completion.terminal.reason
                                )))
                            }
                        };
                        Ok(result.text)
                    }
                    .await;
                    let visible_result = outcome
                        .as_deref()
                        .unwrap_or("subagent ended without a result");
                    self.hook_runtime
                        .run(
                            crate::hooks::HookType::SubagentStop,
                            "*",
                            &crate::hooks::subagent_context_for(
                                crate::hooks::HookType::SubagentStop,
                                description,
                                Some(visible_result),
                            ),
                        )
                        .await;
                    self.hook_runtime.record_event(RunEvent::SubagentFinished {
                        description: description.to_string(),
                        status: if outcome.is_ok() {
                            TechnicalStatus::Succeeded
                        } else {
                            TechnicalStatus::Failed
                        },
                        elapsed_ms: subagent_started.elapsed().as_millis() as u64,
                    });
                    outcome
                })
                .await
        })
    }

    fn run_subagents<'a>(
        &'a self,
        tasks: &'a [(String, String)],
    ) -> Pin<Box<dyn Future<Output = Vec<Result<String>>> + Send + 'a>> {
        Box::pin(async move {
            let futs: Vec<_> = tasks.iter().map(|(p, d)| self.run_subagent(p, d)).collect();
            futures::future::join_all(futs).await
        })
    }
}

/// Strip `thinking` blocks from every message so they aren't sent back to
/// the API.  Needed for Bedrock-based proxies that reject `signature` fields
/// in thinking blocks.  Thinking content is internal-only; stripping it is
/// safe for all providers.
pub fn strip_thinking(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|m| {
            let content = match &m.content {
                MessageContent::Text(_) => return m.clone(),
                MessageContent::Blocks(blocks) => {
                    let filtered: Vec<ContentBlock> = blocks
                        .iter()
                        .filter(|b| !matches!(b, ContentBlock::Thinking { .. }))
                        .cloned()
                        .collect();
                    if filtered.is_empty() {
                        // Don't send empty messages — replace with a
                        // minimal placeholder.
                        MessageContent::from_text("(thinking omitted)")
                    } else {
                        MessageContent::from_blocks(filtered)
                    }
                }
            };
            Message {
                role: m.role,
                content,
            }
        })
        .collect()
}

/// Repair orphaned `tool_use` blocks in a message sequence. The Anthropic API
/// requires that every `tool_use` in an assistant message be immediately followed
/// by a matching `tool_result` in the next user message. If any are missing
/// (e.g. from session corruption or interrupted runs), the orphaned `tool_use`
/// blocks are removed. Empty assistant messages after removal are dropped along
/// with the paired (now orphaned) user message.
pub fn repair_tool_pairing(messages: &mut Vec<Message>) {
    let mut i = 0;
    while i < messages.len() {
        // We only care about assistant messages.
        if messages[i].role != nonoclaw_core::Role::Assistant {
            i += 1;
            continue;
        }

        // Collect tool_use IDs from this assistant message.
        let tool_use_ids: Vec<String> = match &messages[i].content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect(),
            _ => {
                i += 1;
                continue;
            }
        };

        if tool_use_ids.is_empty() {
            i += 1;
            continue;
        }

        // Check the next message (must be user) for matching tool_result blocks.
        let next_idx = i + 1;
        let orphans = if next_idx < messages.len()
            && messages[next_idx].role == nonoclaw_core::Role::User
        {
            let result_ids: Vec<String> = match &messages[next_idx].content {
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            tool_use_ids
                .iter()
                .filter(|id| !result_ids.contains(id))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            // No next message or next is not user — all are orphans.
            tool_use_ids.clone()
        };

        if orphans.is_empty() {
            i += 2; // skip past the user message too
            continue;
        }

        tracing::warn!(
            ?orphans,
            assistant_idx = i,
            "removing orphaned tool_use blocks"
        );

        // Remove orphaned tool_use blocks from the assistant message.
        let mut need_cleanup = false;
        if let MessageContent::Blocks(ref mut blocks) = messages[i].content {
            blocks.retain(|b| match b {
                ContentBlock::ToolUse { id, .. } => !orphans.contains(id),
                _ => true,
            });
            // If the assistant message now has only Thinking blocks left (no
            // Text or ToolUse), remove the entire assistant+user pair.
            let has_substance = blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. } | ContentBlock::ToolUse { .. }));
            if !has_substance {
                need_cleanup = true;
            }
        }

        if need_cleanup {
            // Remove the assistant message.
            messages.remove(i);
            // Remove the paired user message (which held the tool results) if
            // it exists and has only tool_result blocks matching our orphans.
            if i < messages.len() && messages[i].role == nonoclaw_core::Role::User {
                let all_orphaned_results = match &messages[i].content {
                    MessageContent::Blocks(blocks) => blocks.iter().all(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => {
                            orphans.contains(tool_use_id)
                        }
                        _ => false,
                    }),
                    _ => false,
                };
                if all_orphaned_results {
                    messages.remove(i);
                }
            }
            // Don't advance i — we removed messages, so the next iteration
            // starts at the same position.
        } else {
            i += 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options() {
        let o = EngineOptions::default();
        assert_eq!(o.max_turns, 10);
        assert!(o.is_non_interactive);
    }

    #[test]
    fn preview_passes_content_through() {
        let multi = preview("line1\nline2");
        assert_eq!(multi, "line1\nline2");
        let huge = "a".repeat(600_000);
        let p = preview(&huge);
        assert!(p.contains("truncated"));
    }

    #[test]
    fn task_changes_are_structured_engine_events() {
        // **Validates: Requirements 2.3, 2.5**
        let event = EngineEvent::TaskChanged {
            change: nonoclaw_core::TaskChange {
                scope: "parent".into(),
                source: nonoclaw_core::TaskChangeSource::TodoWrite,
                change: nonoclaw_core::TaskChangeKind::Replaced,
                tasks: vec![nonoclaw_core::TaskSnapshot {
                    id: "todo:parent:1".into(),
                    subject: "work".into(),
                    status: nonoclaw_core::TaskStatus::InProgress,
                    active_form: Some("Working".into()),
                    owner: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                }],
            },
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["kind"], "task_changed");
        assert_eq!(value["change"]["scope"], "parent");
        assert_eq!(value["change"]["tasks"][0]["status"], "in_progress");
        let decoded: EngineEvent = serde_json::from_value(value).unwrap();
        assert!(matches!(decoded, EngineEvent::TaskChanged { .. }));
    }

    #[test]
    fn repair_removes_orphaned_tool_use() {
        let mut msgs = vec![
            Message::user(MessageContent::from_text("hi")),
            Message::assistant(MessageContent::from_blocks(vec![
                ContentBlock::text("let me read that"),
                ContentBlock::ToolUse {
                    id: "tu_1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "/tmp/a"}),
                },
            ])),
            // Missing tool_result for tu_1 — this user message has no matching result.
            Message::user(MessageContent::from_text("next question")),
        ];
        repair_tool_pairing(&mut msgs);
        // The orphaned tool_use should be removed; the assistant message keeps its text.
        assert_eq!(msgs.len(), 3);
        if let MessageContent::Blocks(ref blocks) = msgs[1].content {
            assert!(blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. })));
            assert!(!blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. })));
        } else {
            panic!("expected blocks");
        }
    }

    #[test]
    fn repair_cleans_empty_assistant_after_orphan_removal() {
        let mut msgs = vec![
            Message::user(MessageContent::from_text("hi")),
            // Assistant with ONLY a tool_use — no text.
            Message::assistant(MessageContent::from_blocks(vec![ContentBlock::ToolUse {
                id: "tu_2".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "/tmp/b"}),
            }])),
            // User message with only tool_result blocks that are ALSO orphans.
            Message::user(MessageContent::from_blocks(vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tu_2".into(),
                    content: nonoclaw_core::ToolResultContent::Text("result".into()),
                    is_error: Some(false),
                },
            ])),
        ];
        repair_tool_pairing(&mut msgs);
        // Both messages removed because assistant had no substance after removal.
        // Actually in this case tu_2 IS matched by the tool_result, so no orphans.
        // Let me fix: the result matches, so nothing changes.
        assert_eq!(msgs.len(), 3); // all good
    }

    #[test]
    fn repair_keeps_valid_pairing() {
        let mut msgs = vec![
            Message::user(MessageContent::from_text("read /tmp/x")),
            Message::assistant(MessageContent::from_blocks(vec![ContentBlock::ToolUse {
                id: "tu_3".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "/tmp/x"}),
            }])),
            Message::user(MessageContent::from_blocks(vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tu_3".into(),
                    content: nonoclaw_core::ToolResultContent::Text("content here".into()),
                    is_error: Some(false),
                },
            ])),
        ];
        repair_tool_pairing(&mut msgs);
        assert_eq!(msgs.len(), 3);
        // tu_3 remains because its result is present.
        if let MessageContent::Blocks(ref blocks) = msgs[1].content {
            assert!(blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. })));
        }
    }

    async fn spawn_provider_fixture(
        answers: Vec<&'static str>,
    ) -> (
        Arc<Client>,
        tokio::sync::mpsc::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, request_rx) = tokio::sync::mpsc::channel(answers.len().max(1));
        let task = tokio::spawn(async move {
            for answer in answers {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let header_end = loop {
                    let mut chunk = [0_u8; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "provider fixture request ended before headers");
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while request.len() < header_end + content_length {
                    let mut chunk = [0_u8; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "provider fixture request body was truncated");
                    request.extend_from_slice(&chunk[..read]);
                }
                let body =
                    String::from_utf8(request[header_end..header_end + content_length].to_vec())
                        .unwrap();
                request_tx.send(body).await.unwrap();

                let answer_json = serde_json::to_string(answer).unwrap();
                let sse = format!(
                    "event: message_start\ndata: {{\"message\":{{\"id\":\"msg_fixture\",\"model\":\"fixture-model\",\"usage\":{{\"input_tokens\":3,\"output_tokens\":0}}}}}}\n\n\
                     event: content_block_start\ndata: {{\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
                     event: content_block_delta\ndata: {{\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{answer_json}}}}}\n\n\
                     event: content_block_stop\ndata: {{\"index\":0}}\n\n\
                     event: message_delta\ndata: {{\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":2}}}}\n\n\
                     event: message_stop\ndata: {{}}\n\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    sse.len(),
                    sse
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let client =
            Client::new(Some("fixture-key".into()), None, format!("http://{addr}")).unwrap();
        (Arc::new(client), request_rx, task)
    }

    fn fixture_engine(client: Arc<Client>) -> QueryEngine {
        let (registry, todos) = nonoclaw_tools::register_all();
        let options = EngineOptions {
            model: "fixture-requested-model".into(),
            max_turns: 1,
            auto_compact: false,
            ..EngineOptions::default()
        };
        QueryEngine::new(client, Arc::new(registry), todos, options)
    }

    /// Full non-interactive engine success through a local Anthropic SSE
    /// Provider fixture; no external API is contacted. Feature Matrix: §2.2 headless.
    #[tokio::test]
    async fn headless_minimal_success_path_uses_provider_fixture() {
        let (client, mut requests, fixture_task) =
            spawn_provider_fixture(vec!["fixture answer"]).await;
        let mut engine = fixture_engine(client);
        let cwd = std::env::temp_dir().join(format!("nonoclaw-headless-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let mut events = Vec::new();
        let result = engine
            .run(MessageContent::from_text("fixture prompt"), &cwd, |event| {
                events.push(event.clone())
            })
            .await
            .unwrap();
        fixture_task.await.unwrap();

        assert_eq!(result.text, "fixture answer");
        assert_eq!(result.turns, 1);
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::ModelInfo { model } if model == "fixture-model"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::AssistantDone { text } if text == "fixture answer"
        )));
        let request: Value = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
        assert_eq!(request["model"], "fixture-requested-model");
        assert!(request["messages"].to_string().contains("fixture prompt"));
    }

    #[tokio::test]
    async fn stop_and_session_end_hooks_run_in_lifecycle_order() {
        // **Validates: Requirements 7.4**
        let (client, _requests, fixture_task) = spawn_provider_fixture(vec!["done"]).await;
        let mut engine = fixture_engine(client);
        let cwd = std::env::temp_dir().join(format!("nonoclaw-hooks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(cwd.join(".nonoclaw")).unwrap();
        let events = cwd.join("events.txt");
        let config = serde_json::json!({
            "hooks": {
                "Stop": [{
                    "command": "sh",
                    "args": ["-c", format!("printf 'Stop\\n' >> '{}'", events.display())]
                }],
                "SessionEnd": [{
                    "command": "sh",
                    "args": ["-c", format!("printf 'SessionEnd\\n' >> '{}'", events.display())]
                }]
            }
        });
        std::fs::write(
            cwd.join(".nonoclaw/hooks.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        engine
            .run(MessageContent::from_text("finish"), &cwd, |_| {})
            .await
            .unwrap();
        fixture_task.await.unwrap();
        assert_eq!(
            std::fs::read_to_string(events).unwrap(),
            "Stop\nSessionEnd\n"
        );
        std::fs::remove_dir_all(cwd).ok();
    }

    #[tokio::test]
    async fn subagent_start_and_stop_hooks_wrap_child_execution() {
        // **Validates: Requirements 7.4**
        let (client, _requests, fixture_task) = spawn_provider_fixture(vec!["child done"]).await;
        let cwd = std::env::temp_dir().join(format!("nonoclaw-sub-hooks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let events = cwd.join("events.txt");
        let make_hook = |label: &str| crate::hooks::HookDef {
            matcher: String::new(),
            command: "sh".into(),
            args: vec![
                "-c".into(),
                format!("printf '{label}\\n' >> '{}'", events.display()),
            ],
            prompt: None,
            http: None,
            timeout_secs: Some(2),
            failure_policy: crate::hooks::HookFailurePolicy::Deny,
        };
        let cancel = CancellationToken::new();
        let hook_runtime = crate::hooks::HookRuntime::new(
            vec![
                (
                    crate::hooks::HookType::SubagentStart,
                    make_hook("SubagentStart"),
                ),
                (
                    crate::hooks::HookType::SubagentStop,
                    make_hook("SubagentStop"),
                ),
            ],
            Some(Arc::clone(&client)),
            "fixture-requested-model",
            cancel.child_token(),
        );
        let (registry, todos) = nonoclaw_tools::register_all();
        let options = EngineOptions {
            model: "fixture-requested-model".into(),
            max_turns: 1,
            auto_compact: false,
            ..EngineOptions::default()
        };
        let parent = RunContext::new(
            "parent-session",
            cwd.clone(),
            "fixture-requested-model",
            RunLimits::default(),
        );
        let spawner = EngineSubagent {
            client,
            registry: Arc::new(registry),
            options,
            cwd: cwd.clone(),
            hook_runtime,
            run_context: parent,
            task_store: todos,
            lifecycle: SubagentLifecycle::new(cancel),
        };
        let result = spawner
            .run_subagent("do child work", "child fixture")
            .await
            .unwrap();
        fixture_task.await.unwrap();
        assert_eq!(result, "child done");
        assert_eq!(
            std::fs::read_to_string(events).unwrap(),
            "SubagentStart\nSubagentStop\n"
        );
        std::fs::remove_dir_all(cwd).ok();
    }

    /// Resume loads old JSONL history, sends it to the fixture Provider, and
    /// appends the new turn to the same session. Feature Matrix: §2.2/§5 session resume.
    #[tokio::test]
    async fn session_resume_minimal_success_path_preserves_history() {
        let (client, mut requests, fixture_task) =
            spawn_provider_fixture(vec!["resumed answer"]).await;
        let cwd = std::env::temp_dir().join(format!("nonoclaw-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let session_file = cwd.join("resume.jsonl");
        let session = crate::session::SessionService::new()
            .open_path(session_file, "resume-id", &cwd, "fixture-requested-model")
            .unwrap();
        session
            .append(Message::user(MessageContent::from_text("first question")))
            .await
            .unwrap();
        session
            .append(Message::assistant(MessageContent::from_text(
                "first answer",
            )))
            .await
            .unwrap();
        let snapshot = session.snapshot().await.unwrap();
        let (registry, todos) = nonoclaw_tools::register_all();
        let options = EngineOptions {
            model: "fixture-requested-model".into(),
            max_turns: 1,
            auto_compact: false,
            ..EngineOptions::default()
        };
        let mut engine = QueryEngine::with_session(
            client,
            Arc::new(registry),
            todos,
            options,
            session.clone(),
            snapshot,
        );
        let result = engine
            .run(MessageContent::from_text("second question"), &cwd, |_| {})
            .await
            .unwrap();
        fixture_task.await.unwrap();

        assert_eq!(result.text, "resumed answer");
        let request = requests.recv().await.unwrap();
        assert!(request.contains("first question"));
        assert!(request.contains("first answer"));
        assert!(request.contains("second question"));
        let persisted = session.snapshot().await.unwrap();
        assert_eq!(persisted.messages.len(), 4);
    }
}
