//! Hook loading and execution for command, prompt, and HTTP actions.
//!
//! User hooks are loaded before project hooks. A project hook with the same
//! event type and matcher replaces the user hook, preserving the historical
//! override contract.

use nonoclaw_api::{Client, RequestParams, SystemBlock};
use nonoclaw_core::{
    ContentBlock, Message, MessageContent, PermissionDecision, RunEvent, TechnicalStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookType {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
}

impl fmt::Display for HookType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HookType::PreToolUse => "PreToolUse",
            HookType::PostToolUse => "PostToolUse",
            HookType::PostToolUseFailure => "PostToolUseFailure",
            HookType::Notification => "Notification",
            HookType::UserPromptSubmit => "UserPromptSubmit",
            HookType::SessionStart => "SessionStart",
            HookType::SessionEnd => "SessionEnd",
            HookType::Stop => "Stop",
            HookType::SubagentStart => "SubagentStart",
            HookType::SubagentStop => "SubagentStop",
            HookType::PreCompact => "PreCompact",
            HookType::PostCompact => "PostCompact",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailurePolicy {
    /// Preserve the legacy best-effort behavior when an action cannot run.
    #[default]
    Continue,
    /// Fail closed. For a decision hook this becomes a denial; lifecycle hooks
    /// log the failure and continue their enclosing lifecycle.
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    #[serde(default)]
    pub matcher: String,
    /// Shell command (legacy and still fully supported).
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub prompt: Option<PromptHookConfig>,
    #[serde(default)]
    pub http: Option<HttpHookConfig>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub failure_policy: HookFailurePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptHookConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpHookConfig {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// One validated action kind. `HookDef` keeps the old on-disk shape while all
/// runners dispatch through this single representation.
#[derive(Debug, Clone)]
pub enum HookAction {
    Command { command: String, args: Vec<String> },
    Prompt(PromptHookConfig),
    Http(HttpHookConfig),
}

impl HookDef {
    fn action(&self) -> Result<HookAction, String> {
        let command = (!self.command.trim().is_empty()).then(|| HookAction::Command {
            command: self.command.clone(),
            args: self.args.clone(),
        });
        let actions = usize::from(command.is_some())
            + usize::from(self.prompt.is_some())
            + usize::from(self.http.is_some());
        if actions != 1 {
            return Err(if actions == 0 {
                "hook must declare exactly one supported action: command, prompt, or http".into()
            } else {
                "hook declares multiple actions; choose exactly one of command, prompt, or http"
                    .into()
            });
        }
        if let Some(action) = command {
            return Ok(action);
        }
        if let Some(prompt) = &self.prompt {
            return Ok(HookAction::Prompt(prompt.clone()));
        }
        let http = self.http.as_ref().expect("one validated action");
        let expanded = expand_env(&http.url).map_err(|_| {
            "HTTP hook URL references a missing or malformed environment variable".to_string()
        })?;
        let url = reqwest::Url::parse(&expanded)
            .map_err(|_| "HTTP hook URL is invalid (expected http:// or https://)".to_string())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("HTTP hook URL must use http or https".into());
        }
        Ok(HookAction::Http(http.clone()))
    }

    fn timeout(&self, action: &HookAction) -> Duration {
        let seconds = self
            .timeout_secs
            .or(match action {
                HookAction::Prompt(config) => config.timeout_secs,
                _ => None,
            })
            .unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS)
            .max(1);
        Duration::from_secs(seconds)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HooksFile {
    #[serde(default)]
    hooks: TypedHooks,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct TypedHooks {
    #[serde(default)]
    pre_tool_use: Vec<HookDef>,
    #[serde(default)]
    post_tool_use: Vec<HookDef>,
    #[serde(default)]
    post_tool_use_failure: Vec<HookDef>,
    #[serde(default)]
    notification: Vec<HookDef>,
    #[serde(default)]
    user_prompt_submit: Vec<HookDef>,
    #[serde(default)]
    session_start: Vec<HookDef>,
    #[serde(default)]
    session_end: Vec<HookDef>,
    #[serde(default)]
    stop: Vec<HookDef>,
    #[serde(default)]
    subagent_start: Vec<HookDef>,
    #[serde(default)]
    subagent_stop: Vec<HookDef>,
    #[serde(default)]
    pre_compact: Vec<HookDef>,
    #[serde(default)]
    post_compact: Vec<HookDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDiagnostic {
    pub path: PathBuf,
    pub hook_type: Option<HookType>,
    pub matcher: Option<String>,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct HookLoadReport {
    pub hooks: Vec<(HookType, HookDef)>,
    pub diagnostics: Vec<HookDiagnostic>,
}

/// Compatibility entry point. Invalid actions are rejected during loading and
/// surfaced as diagnostics rather than being silently ignored.
pub fn load_hooks(cwd: &Path) -> Vec<(HookType, HookDef)> {
    let report = load_hooks_with_diagnostics(cwd);
    for diagnostic in &report.diagnostics {
        tracing::warn!(
            path = %diagnostic.path.display(),
            hook_type = ?diagnostic.hook_type,
            matcher = ?diagnostic.matcher,
            message = %diagnostic.message,
            "hook configuration rejected"
        );
    }
    report.hooks
}

pub fn load_hooks_with_diagnostics(cwd: &Path) -> HookLoadReport {
    let mut report = HookLoadReport::default();
    if let Some(home) = nonoclaw_core::nonoclaw_data_dir() {
        load_hook_file(&home.join("hooks.json"), &mut report);
    }
    load_hook_file(&cwd.join(".nonoclaw").join("hooks.json"), &mut report);

    report.hooks = merge_hooks(report.hooks);
    report
}

fn merge_hooks(hooks: Vec<(HookType, HookDef)>) -> Vec<(HookType, HookDef)> {
    // Later entries (project, then later duplicates) replace earlier entries.
    let mut merged: Vec<(HookType, HookDef)> = Vec::new();
    for (hook_type, hook) in hooks {
        if let Some(index) = merged.iter().position(|(existing_type, existing)| {
            *existing_type == hook_type && existing.matcher == hook.matcher
        }) {
            merged[index] = (hook_type, hook);
        } else {
            merged.push((hook_type, hook));
        }
    }
    merged
}

fn load_hook_file(path: &Path, report: &mut HookLoadReport) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let hooks = match parse_hooks_json(&text) {
        Ok(hooks) => hooks,
        Err(message) => {
            report.diagnostics.push(HookDiagnostic {
                path: path.to_path_buf(),
                hook_type: None,
                matcher: None,
                message,
            });
            return;
        }
    };
    for (hook_type, hook) in flatten_hooks(hooks) {
        match hook.action() {
            Ok(_) => report.hooks.push((hook_type, hook)),
            Err(message) => report.diagnostics.push(HookDiagnostic {
                path: path.to_path_buf(),
                hook_type: Some(hook_type),
                matcher: Some(hook.matcher.clone()),
                message,
            }),
        }
    }
}

fn parse_hooks_json(text: &str) -> Result<TypedHooks, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("invalid hooks JSON: {error}"))?;
    if value.get("hooks").is_some() {
        serde_json::from_value::<HooksFile>(value)
            .map(|file| file.hooks)
            .map_err(|error| format!("invalid hooks object: {error}"))
    } else {
        serde_json::from_value::<TypedHooks>(value)
            .map_err(|error| format!("invalid hooks object: {error}"))
    }
}

fn flatten_hooks(hooks: TypedHooks) -> Vec<(HookType, HookDef)> {
    let mut output = Vec::new();
    macro_rules! append {
        ($field:ident, $kind:expr) => {
            output.extend(hooks.$field.into_iter().map(|hook| ($kind, hook)));
        };
    }
    append!(pre_tool_use, HookType::PreToolUse);
    append!(post_tool_use, HookType::PostToolUse);
    append!(post_tool_use_failure, HookType::PostToolUseFailure);
    append!(notification, HookType::Notification);
    append!(user_prompt_submit, HookType::UserPromptSubmit);
    append!(session_start, HookType::SessionStart);
    append!(session_end, HookType::SessionEnd);
    append!(stop, HookType::Stop);
    append!(subagent_start, HookType::SubagentStart);
    append!(subagent_stop, HookType::SubagentStop);
    append!(pre_compact, HookType::PreCompact);
    append!(post_compact, HookType::PostCompact);
    output
}

#[derive(Debug, Clone, PartialEq)]
pub enum HookDecision {
    Allow { updated_input: Option<Value> },
    Deny { reason: String },
    Ask { message: String },
}

impl HookDecision {
    fn into_permission(self) -> PermissionDecision {
        match self {
            HookDecision::Allow { updated_input } => PermissionDecision::Allow { updated_input },
            HookDecision::Deny { reason } => PermissionDecision::deny(reason),
            HookDecision::Ask { message } => PermissionDecision::ask(message),
        }
    }
}

#[derive(Debug)]
enum HookRunError {
    Cancelled,
    Timeout,
    Unavailable(&'static str),
    Failed(&'static str),
    InvalidDecision,
}

/// Runtime shared by tool and lifecycle hook call sites.
#[derive(Clone)]
pub struct HookRuntime {
    hooks: Arc<Vec<(HookType, HookDef)>>,
    prompt_client: Option<Arc<Client>>,
    default_model: String,
    cancel: CancellationToken,
    http: reqwest::Client,
    events: Arc<Mutex<Vec<RunEvent>>>,
}

impl HookRuntime {
    pub fn new(
        hooks: Vec<(HookType, HookDef)>,
        prompt_client: Option<Arc<Client>>,
        default_model: impl Into<String>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            hooks: Arc::new(hooks),
            prompt_client,
            default_model: default_model.into(),
            cancel,
            http: reqwest::Client::new(),
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn record_event(&self, event: RunEvent) {
        self.events.lock().unwrap().push(event.redacted());
    }

    pub fn drain_events(&self) -> Vec<RunEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    pub fn definitions(&self) -> &[(HookType, HookDef)] {
        self.hooks.as_slice()
    }

    /// Run matching hooks for lifecycle events. Decisions are recorded but do
    /// not alter an already-completed lifecycle transition.
    pub async fn run(&self, hook_type: HookType, matcher: &str, context: &Value) {
        let _ = self
            .run_matching(hook_type, matcher, context.clone(), false)
            .await;
    }

    /// Run matching decision hooks in order. Updated input is passed to later
    /// hooks and returned to the canonical ToolExecutor.
    pub async fn decide(
        &self,
        hook_type: HookType,
        matcher: &str,
        context: &Value,
    ) -> PermissionDecision {
        self.run_matching(hook_type, matcher, context.clone(), true)
            .await
            .into_permission()
    }

    async fn run_matching(
        &self,
        hook_type: HookType,
        matcher: &str,
        mut context: Value,
        decision_event: bool,
    ) -> HookDecision {
        if self.cancel.is_cancelled() {
            return HookDecision::Deny {
                reason: "hook execution cancelled".into(),
            };
        }
        let mut updated_input = None;
        for (configured_type, hook) in self.hooks.iter() {
            if *configured_type != hook_type || !simple_match(&hook.matcher, matcher) {
                continue;
            }
            let action = match hook.action() {
                Ok(action) => action,
                Err(_) => continue, // load validation already reported this
            };
            let started = Instant::now();
            self.record_event(RunEvent::HookStarted {
                hook_type: hook_type.to_string(),
                action: action_name(&action).into(),
                matcher: hook.matcher.clone(),
            });
            let result = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Err(HookRunError::Cancelled),
                result = tokio::time::timeout(
                    hook.timeout(&action),
                    self.execute_action(&action, hook_type, &context),
                ) => result.unwrap_or(Err(HookRunError::Timeout)),
            };
            let elapsed_ms = started.elapsed().as_millis() as u64;
            self.record_event(RunEvent::HookFinished {
                hook_type: hook_type.to_string(),
                action: action_name(&action).into(),
                matcher: hook.matcher.clone(),
                status: match &result {
                    Ok(HookDecision::Allow { .. }) => TechnicalStatus::Succeeded,
                    Ok(HookDecision::Deny { .. }) => TechnicalStatus::Denied,
                    Ok(HookDecision::Ask { .. }) => TechnicalStatus::Waiting,
                    Err(HookRunError::Cancelled) => TechnicalStatus::Cancelled,
                    Err(_) => TechnicalStatus::Failed,
                },
                elapsed_ms,
            });
            tracing::debug!(
                hook_type = %hook_type,
                matcher = %hook.matcher,
                action = action_name(&action),
                elapsed_ms,
                ok = result.is_ok(),
                "hook action finished (payload, headers, output, and credentials redacted)"
            );
            match result {
                Ok(HookDecision::Allow {
                    updated_input: action_input,
                }) => {
                    if let Some(input) = action_input {
                        if let Some(object) = context.as_object_mut() {
                            object.insert("tool_input".into(), input.clone());
                        }
                        updated_input = Some(input);
                    }
                }
                Ok(HookDecision::Deny { reason }) if decision_event => {
                    return HookDecision::Deny { reason };
                }
                Ok(HookDecision::Ask { message }) if decision_event => {
                    return HookDecision::Ask { message };
                }
                Ok(_) => {}
                Err(HookRunError::Cancelled) => {
                    return HookDecision::Deny {
                        reason: "hook execution cancelled".into(),
                    };
                }
                Err(error) => {
                    tracing::warn!(
                        hook_type = %hook_type,
                        matcher = %hook.matcher,
                        category = hook_error_name(&error),
                        "hook action failed (details redacted)"
                    );
                    if decision_event && matches!(hook.failure_policy, HookFailurePolicy::Deny) {
                        return HookDecision::Deny {
                            reason: format!("{hook_type} hook failed closed"),
                        };
                    }
                }
            }
        }
        HookDecision::Allow { updated_input }
    }

    async fn execute_action(
        &self,
        action: &HookAction,
        hook_type: HookType,
        context: &Value,
    ) -> Result<HookDecision, HookRunError> {
        match action {
            HookAction::Command { command, args } => {
                execute_command(command, args, hook_type, context).await
            }
            HookAction::Prompt(config) => self.execute_prompt(config, hook_type, context).await,
            HookAction::Http(config) => self.execute_http(config, context).await,
        }
    }

    async fn execute_prompt(
        &self,
        config: &PromptHookConfig,
        hook_type: HookType,
        context: &Value,
    ) -> Result<HookDecision, HookRunError> {
        let client = self
            .prompt_client
            .as_ref()
            .ok_or(HookRunError::Unavailable("prompt client"))?;
        let model = config.model.as_deref().unwrap_or(&self.default_model);
        let params = RequestParams {
            model: model.to_string(),
            max_tokens: 256,
            system: vec![SystemBlock {
                kind: "text".into(),
                text: "Evaluate this hook event. Return JSON only: {\"decision\":\"allow\"}, {\"decision\":\"deny\",\"reason\":\"...\"}, {\"decision\":\"ask\",\"message\":\"...\"}, or allow with updated_input.".into(),
                cache_control: None,
            }],
            messages: vec![Message::user(MessageContent::from_text(
                &serde_json::to_string(context).map_err(|_| HookRunError::Failed("serialize"))?,
            ))],
            tools: Vec::new(),
            tool_choice: None,
            thinking: None,
            temperature: Some(0.0),
            betas: Vec::new(),
            trace_label: Some(format!("hook-{hook_type}")),
        };
        let output = client
            .run_turn_with_cancel(&params, |_| {}, self.cancel.child_token())
            .await
            .map_err(|_| HookRunError::Failed("prompt request"))?;
        let text: String = output
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parse_decision_text(&text, true)
    }

    async fn execute_http(
        &self,
        config: &HttpHookConfig,
        context: &Value,
    ) -> Result<HookDecision, HookRunError> {
        let url = expand_env(&config.url).map_err(|_| HookRunError::Failed("environment"))?;
        let mut request = self.http.post(url).json(context);
        for (name, value) in &config.headers {
            let value = expand_env(value).map_err(|_| HookRunError::Failed("environment"))?;
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| HookRunError::Failed("HTTP request"))?;
        if !response.status().is_success() {
            return Err(HookRunError::Failed("HTTP status"));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| HookRunError::Failed("HTTP body"))?;
        if bytes.is_empty() {
            return Ok(HookDecision::Allow {
                updated_input: None,
            });
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| HookRunError::InvalidDecision)?;
        parse_decision_value(&value)
    }
}

async fn execute_command(
    command: &str,
    args: &[String],
    hook_type: HookType,
    context: &Value,
) -> Result<HookDecision, HookRunError> {
    let payload = serde_json::to_vec(context).map_err(|_| HookRunError::Failed("serialize"))?;
    let mut child = tokio::process::Command::new(command)
        .args(args)
        .env("NONOCLAW_HOOK_EVENT", hook_type.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| HookRunError::Failed("spawn"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&payload)
            .await
            .map_err(|_| HookRunError::Failed("stdin"))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|_| HookRunError::Failed("wait"))?;
    if !output.status.success() {
        return Ok(HookDecision::Deny {
            reason: format!("{hook_type} command hook denied the operation"),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_decision_text(&stdout, false)
}

fn parse_decision_text(text: &str, require_json: bool) -> Result<HookDecision, HookRunError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return if require_json {
            Err(HookRunError::InvalidDecision)
        } else {
            Ok(HookDecision::Allow {
                updated_input: None,
            })
        };
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => parse_decision_value(&value),
        Err(_) if !require_json => Ok(HookDecision::Allow {
            updated_input: None,
        }),
        Err(_) => Err(HookRunError::InvalidDecision),
    }
}

fn parse_decision_value(value: &Value) -> Result<HookDecision, HookRunError> {
    if let Some(decision) = value.get("decision").and_then(Value::as_str) {
        return match decision.to_ascii_lowercase().as_str() {
            "allow" => Ok(HookDecision::Allow {
                updated_input: value
                    .get("updated_input")
                    .or_else(|| value.get("updatedInput"))
                    .cloned(),
            }),
            "deny" => Ok(HookDecision::Deny {
                reason: value
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("hook denied the operation")
                    .to_string(),
            }),
            "ask" => Ok(HookDecision::Ask {
                message: value
                    .get("message")
                    .or_else(|| value.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("hook requires confirmation")
                    .to_string(),
            }),
            _ => Err(HookRunError::InvalidDecision),
        };
    }
    // Backward-compatible prompt schema documented by NonoClaw: {ok, reason?}.
    if let Some(ok) = value.get("ok").and_then(Value::as_bool) {
        return if ok {
            Ok(HookDecision::Allow {
                updated_input: value
                    .get("updated_input")
                    .or_else(|| value.get("updatedInput"))
                    .cloned(),
            })
        } else {
            Ok(HookDecision::Deny {
                reason: value
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("hook denied the operation")
                    .to_string(),
            })
        };
    }
    Err(HookRunError::InvalidDecision)
}

fn action_name(action: &HookAction) -> &'static str {
    match action {
        HookAction::Command { .. } => "command",
        HookAction::Prompt(_) => "prompt",
        HookAction::Http(_) => "http",
    }
}

fn hook_error_name(error: &HookRunError) -> &'static str {
    match error {
        HookRunError::Cancelled => "cancelled",
        HookRunError::Timeout => "timeout",
        HookRunError::Unavailable(detail) => {
            let _ = detail;
            "unavailable"
        }
        HookRunError::Failed(detail) => {
            let _ = detail;
            "failed"
        }
        HookRunError::InvalidDecision => "invalid_decision",
    }
}

fn expand_env(input: &str) -> Result<String, ()> {
    let mut output = String::with_capacity(input.len());
    let mut remainder = input;
    while let Some(start) = remainder.find("${") {
        output.push_str(&remainder[..start]);
        let after = &remainder[start + 2..];
        let end = after.find('}').ok_or(())?;
        let name = &after[..end];
        if name.is_empty()
            || !name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(());
        }
        output.push_str(&std::env::var(name).map_err(|_| ())?);
        remainder = &after[end + 1..];
    }
    output.push_str(remainder);
    Ok(output)
}

/// Empty matchers historically meant "all" for lifecycle hooks.
fn simple_match(pattern: &str, text: &str) -> bool {
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return text.starts_with(prefix);
    }
    pattern == text
}

pub fn tool_context_for(
    event: HookType,
    tool_name: &str,
    input: &Value,
    result: Option<&str>,
) -> Value {
    let mut context = json!({
        "tool_name": tool_name,
        "tool_input": input,
        "hook_event_name": event.to_string()
    });
    if let Some(result) = result {
        context["tool_result"] = Value::String(redact_text(result));
    }
    context
}

pub fn tool_context(tool_name: &str, input: &Value) -> Value {
    tool_context_for(HookType::PreToolUse, tool_name, input, None)
}

pub fn prompt_context(prompt: &str) -> Value {
    json!({
        "prompt": prompt,
        "hook_event_name": HookType::UserPromptSubmit.to_string()
    })
}

pub fn lifecycle_context(event: &str) -> Value {
    json!({ "hook_event_name": event })
}

pub fn compact_context_for(
    event: HookType,
    removed: usize,
    kept: usize,
    before: usize,
    after: usize,
) -> Value {
    json!({
        "removed": removed,
        "kept": kept,
        "tokens_before": before,
        "tokens_after": after,
        "hook_event_name": event.to_string()
    })
}

pub fn compact_context(removed: usize, kept: usize, before: usize, after: usize) -> Value {
    compact_context_for(HookType::PreCompact, removed, kept, before, after)
}

pub fn subagent_context_for(event: HookType, description: &str, result: Option<&str>) -> Value {
    json!({
        "description": description,
        "result": result.map(redact_text),
        "hook_event_name": event.to_string()
    })
}

pub fn subagent_context(description: &str, result_text: &str) -> Value {
    subagent_context_for(HookType::SubagentStop, description, Some(result_text))
}

fn redact_text(text: &str) -> String {
    const MAX_LOGGED_RESULT_CHARS: usize = 2_000;
    let mut redacted: String = text.chars().take(MAX_LOGGED_RESULT_CHARS).collect();
    if text.chars().count() > MAX_LOGGED_RESULT_CHARS {
        redacted.push_str("…[redacted/truncated]");
    }
    for marker in ["sk-", "Bearer ", "x-api-key"] {
        if let Some(index) = redacted.find(marker) {
            redacted.truncate(index);
            redacted.push_str("[REDACTED]");
        }
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn temp_project() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nc-hooks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".nonoclaw")).unwrap();
        dir
    }

    #[test]
    fn loader_covers_all_types_and_diagnoses_unsupported_actions() {
        // **Validates: Requirements 7.3, 7.4**
        let dir = temp_project();
        std::fs::write(
            dir.join(".nonoclaw/hooks.json"),
            r#"{"hooks":{
                "PreToolUse":[{"command":"true"}],
                "PostToolUse":[{"prompt":{}}],
                "PostToolUseFailure":[{"http":{"url":"http://127.0.0.1:9"}}],
                "Notification":[{"command":"true"}],
                "UserPromptSubmit":[{"command":"true"}],
                "SessionStart":[{"command":"true"}],
                "SessionEnd":[{"command":"true"}],
                "Stop":[{"command":"true"}],
                "SubagentStart":[{"command":"true"}],
                "SubagentStop":[{"command":"true"}],
                "PreCompact":[{"command":"true"}],
                "PostCompact":[{"command":"true"},{"matcher":"bad"}]
            }}"#,
        )
        .unwrap();
        let report = load_hooks_with_diagnostics(&dir);
        for hook_type in [
            HookType::PreToolUse,
            HookType::PostToolUse,
            HookType::PostToolUseFailure,
            HookType::Notification,
            HookType::UserPromptSubmit,
            HookType::SessionStart,
            HookType::SessionEnd,
            HookType::Stop,
            HookType::SubagentStart,
            HookType::SubagentStop,
            HookType::PreCompact,
            HookType::PostCompact,
        ] {
            assert!(report.hooks.iter().any(|(loaded, _)| *loaded == hook_type));
        }
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].hook_type, Some(HookType::PostCompact));
        assert!(report.diagnostics[0].message.contains("supported action"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn decision_protocol_covers_allow_deny_ask_and_updated_input() {
        // **Validates: Requirements 7.5**
        assert_eq!(
            parse_decision_text(r#"{"decision":"allow"}"#, true).unwrap(),
            HookDecision::Allow {
                updated_input: None
            }
        );
        assert!(matches!(
            parse_decision_text(r#"{"decision":"deny","reason":"no"}"#, true).unwrap(),
            HookDecision::Deny { reason } if reason == "no"
        ));
        assert!(matches!(
            parse_decision_text(r#"{"decision":"ask","message":"confirm"}"#, true).unwrap(),
            HookDecision::Ask { message } if message == "confirm"
        ));
        assert!(matches!(
            parse_decision_text(
                r#"{"ok":true,"updatedInput":{"command":"safe"}}"#,
                true
            )
            .unwrap(),
            HookDecision::Allow { updated_input: Some(input) } if input["command"] == "safe"
        ));
    }

    #[tokio::test]
    async fn command_action_updates_input_and_obeys_timeout_policy() {
        // **Validates: Requirements 7.3, 7.5, 9.1, 9.5**
        let hooks = vec![(
            HookType::PreToolUse,
            HookDef {
                matcher: "Bash".into(),
                command: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf '{\"decision\":\"allow\",\"updated_input\":{\"command\":\"safe\"}}'"
                        .into(),
                ],
                prompt: None,
                http: None,
                timeout_secs: Some(1),
                failure_policy: HookFailurePolicy::Deny,
            },
        )];
        let runtime = HookRuntime::new(hooks, None, "unused", CancellationToken::new());
        let decision = runtime
            .decide(
                HookType::PreToolUse,
                "Bash",
                &tool_context("Bash", &json!({"command":"unsafe"})),
            )
            .await;
        assert!(matches!(
            decision,
            PermissionDecision::Allow { updated_input: Some(input) } if input["command"] == "safe"
        ));
    }

    #[test]
    fn later_project_definition_replaces_user_definition_by_type_and_matcher() {
        // **Validates: Requirements 7.2**
        let user = HookDef {
            matcher: "Bash*".into(),
            command: "user-hook".into(),
            args: Vec::new(),
            prompt: None,
            http: None,
            timeout_secs: None,
            failure_policy: HookFailurePolicy::Continue,
        };
        let mut project = user.clone();
        project.command = "project-hook".into();
        let merged = merge_hooks(vec![
            (HookType::PreToolUse, user),
            (HookType::PreToolUse, project),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].1.command, "project-hook");
    }

    #[tokio::test]
    async fn prompt_action_uses_local_provider_and_returns_deny() {
        // **Validates: Requirements 7.3, 7.5**
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 16384];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("local-hook-model"));
            let decision =
                serde_json::to_string(r#"{"decision":"deny","reason":"local model denied"}"#)
                    .unwrap();
            let body = format!(
                "event: message_start\ndata: {{\"message\":{{\"id\":\"hook\",\"model\":\"local\",\"usage\":{{}}}}}}\n\n\
                 event: content_block_start\ndata: {{\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
                 event: content_block_delta\ndata: {{\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{decision}}}}}\n\n\
                 event: content_block_stop\ndata: {{\"index\":0}}\n\n\
                 event: message_delta\ndata: {{\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{}}}}\n\n\
                 event: message_stop\ndata: {{}}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let client = Arc::new(
            Client::new(
                Some("local-test-key".into()),
                None,
                format!("http://{address}"),
            )
            .unwrap(),
        );
        let hooks = vec![(
            HookType::PreToolUse,
            HookDef {
                matcher: "*".into(),
                command: String::new(),
                args: Vec::new(),
                prompt: Some(PromptHookConfig {
                    model: Some("local-hook-model".into()),
                    timeout_secs: Some(2),
                }),
                http: None,
                timeout_secs: None,
                failure_policy: HookFailurePolicy::Deny,
            },
        )];
        let runtime = HookRuntime::new(hooks, Some(client), "default", CancellationToken::new());
        let decision = runtime
            .decide(
                HookType::PreToolUse,
                "Read",
                &tool_context("Read", &json!({"file_path":"/tmp/a"})),
            )
            .await;
        server.await.unwrap();
        assert!(matches!(
            decision,
            PermissionDecision::Deny { reason } if reason == "local model denied"
        ));
    }

    #[tokio::test]
    async fn timeout_and_cancellation_fail_closed_without_leaking_output() {
        // **Validates: Requirements 7.3, 9.1, 9.5**
        let timed_out = HookRuntime::new(
            vec![(
                HookType::PreToolUse,
                HookDef {
                    matcher: "*".into(),
                    command: "sh".into(),
                    args: vec!["-c".into(), "sleep 5".into()],
                    prompt: None,
                    http: None,
                    timeout_secs: Some(1),
                    failure_policy: HookFailurePolicy::Deny,
                },
            )],
            None,
            "unused",
            CancellationToken::new(),
        );
        let started = Instant::now();
        let decision = timed_out
            .decide(
                HookType::PreToolUse,
                "Read",
                &tool_context("Read", &json!({})),
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(matches!(decision, PermissionDecision::Deny { .. }));

        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = HookRuntime::new(Vec::new(), None, "unused", cancel);
        assert!(matches!(
            cancelled
                .decide(
                    HookType::PreToolUse,
                    "Read",
                    &tool_context("Read", &json!({})),
                )
                .await,
            PermissionDecision::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn http_action_uses_loopback_and_returns_ask() {
        // **Validates: Requirements 7.3, 7.5**
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 8192];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("PreToolUse"));
            let body = r#"{"decision":"ask","message":"approve local hook"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let hooks = vec![(
            HookType::PreToolUse,
            HookDef {
                matcher: "*".into(),
                command: String::new(),
                args: Vec::new(),
                prompt: None,
                http: Some(HttpHookConfig {
                    url: format!("http://{address}/hook"),
                    headers: HashMap::new(),
                }),
                timeout_secs: Some(2),
                failure_policy: HookFailurePolicy::Deny,
            },
        )];
        let runtime = HookRuntime::new(hooks, None, "unused", CancellationToken::new());
        let decision = runtime
            .decide(
                HookType::PreToolUse,
                "Read",
                &tool_context("Read", &json!({"file_path":"/tmp/a"})),
            )
            .await;
        server.await.unwrap();
        assert!(matches!(
            decision,
            PermissionDecision::Ask { message } if message == "approve local hook"
        ));
    }

    #[test]
    fn matcher_is_total_for_prefix_exact_and_default_patterns() {
        for text in ["", "Read", "Bash", "任意"] {
            assert!(simple_match("*", text));
            assert!(simple_match("", text));
        }
        assert!(simple_match("Bash*", "BashCommand"));
        assert!(simple_match("Read", "Read"));
        assert!(!simple_match("Read", "Write"));
    }
}
