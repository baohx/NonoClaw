//! Canonical tool execution pipeline.
//!
//! [`ToolExecutor`] owns lookup, validation, permission resolution, hooks,
//! bounded scheduling, invocation, result normalization, and per-call trace
//! records. The engine decides *when* tools run; this module decides *how*.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use nonoclaw_core::{PermissionDecision, ValidationResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::background::BackgroundTaskRegistry;
use crate::permissions::PermissionGate;
use crate::registry::ToolRegistry;
use crate::tool::{QuestionResolver, SubagentRunner, ToolCtx, ToolOptions};

const DEFAULT_MAX_CONCURRENCY: usize = 10;
const SUMMARY_SUFFIX_RESERVE: usize = 512;

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Shared safety facts used by permission, scheduling, trace, and future UI
/// adapters. Values are derived once from the resolved tool and its input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRiskMetadata {
    pub read_only: bool,
    pub concurrency_safe: bool,
    pub destructive: bool,
    pub accesses_network: bool,
    pub writes_or_overwrites: bool,
    pub paths: Vec<String>,
    pub command: Option<String>,
    pub network_targets: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTraceStage {
    Lookup,
    Validate,
    PermissionRequest,
    Permission,
    PreHook,
    Call,
    PostHook,
    Normalize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTraceRecord {
    pub stage: ToolTraceStage,
    pub ok: bool,
    pub detail: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
    pub task_changes: Vec<nonoclaw_core::TaskChange>,
    pub metadata: Option<ToolRiskMetadata>,
    pub trace: Vec<ToolTraceRecord>,
    pub local_reference: Option<PathBuf>,
    pub original_chars: usize,
}

#[derive(Debug, Clone)]
pub struct ToolPermissionRequest {
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: Value,
    pub message: String,
}

pub type PermissionResolverFuture =
    Pin<Box<dyn Future<Output = PermissionDecision> + Send + 'static>>;

pub trait ToolPermissionResolver: Send + Sync {
    fn resolve(&self, request: ToolPermissionRequest) -> PermissionResolverFuture;
}

#[async_trait]
pub trait ToolHookRunner: Send + Sync {
    async fn pre_tool_use(&self, _tool_name: &str, _input: &Value) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn post_tool_use(&self, _tool_name: &str, _input: &Value, _success: bool) {}
}

#[derive(Default)]
pub struct NoopToolHooks;

#[async_trait]
impl ToolHookRunner for NoopToolHooks {}

pub struct ToolExecutionContext<'a> {
    pub cwd: &'a Path,
    pub options: &'a ToolOptions,
    pub cancel: &'a CancellationToken,
    pub task_scope: Option<&'a str>,
    pub subagent: Option<&'a dyn SubagentRunner>,
    pub question: Option<&'a dyn QuestionResolver>,
    pub background_registry: Option<Arc<Mutex<BackgroundTaskRegistry>>>,
    pub is_non_interactive: bool,
}

pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
    gate: PermissionGate,
    hooks: Arc<dyn ToolHookRunner>,
    permission_resolver: Option<Arc<dyn ToolPermissionResolver>>,
    max_concurrency: usize,
}

impl ToolExecutor {
    pub fn new(
        registry: Arc<ToolRegistry>,
        gate: PermissionGate,
        hooks: Arc<dyn ToolHookRunner>,
        permission_resolver: Option<Arc<dyn ToolPermissionResolver>>,
        max_concurrency: usize,
    ) -> Self {
        Self {
            registry,
            gate,
            hooks,
            permission_resolver,
            max_concurrency: max_concurrency.max(1),
        }
    }

    pub fn from_env(
        registry: Arc<ToolRegistry>,
        gate: PermissionGate,
        hooks: Arc<dyn ToolHookRunner>,
        permission_resolver: Option<Arc<dyn ToolPermissionResolver>>,
    ) -> Self {
        Self::new(
            registry,
            gate,
            hooks,
            permission_resolver,
            max_tool_concurrency_from_env(),
        )
    }

    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    /// Execute calls with serial barriers around every non-concurrency-safe
    /// call. Safe calls in each consecutive batch are bounded by the configured
    /// cap. Results are always restored to model call order.
    pub async fn execute(
        &self,
        calls: &[ToolCall],
        context: &ToolExecutionContext<'_>,
    ) -> Vec<ToolExecutionResult> {
        let batches = partition_calls(calls, &self.registry);
        let mut ordered: Vec<Option<ToolExecutionResult>> = vec![None; calls.len()];

        for batch in batches {
            if batch.concurrency_safe {
                let completed = stream::iter(batch.indices.into_iter().map(|index| async move {
                    (index, self.execute_one(&calls[index], context).await)
                }))
                .buffer_unordered(self.max_concurrency)
                .collect::<Vec<_>>()
                .await;
                for (index, result) in completed {
                    ordered[index] = Some(result);
                }
            } else {
                for index in batch.indices {
                    ordered[index] = Some(self.execute_one(&calls[index], context).await);
                }
            }
        }

        ordered
            .into_iter()
            .map(|result| result.expect("every partitioned tool call must execute"))
            .collect()
    }

    async fn execute_one(
        &self,
        call: &ToolCall,
        context: &ToolExecutionContext<'_>,
    ) -> ToolExecutionResult {
        let mut trace = Vec::new();
        let Some(tool) = self.registry.find(&call.name) else {
            trace.push(trace_record(
                ToolTraceStage::Lookup,
                false,
                "tool not found",
            ));
            self.hooks
                .post_tool_use(&call.name, &call.input, false)
                .await;
            return ToolExecutionResult {
                id: call.id.clone(),
                name: call.name.clone(),
                content: format!("Unknown tool: {}", call.name),
                is_error: true,
                task_changes: Vec::new(),
                metadata: None,
                trace,
                local_reference: None,
                original_chars: 0,
            };
        };
        trace.push(trace_record(ToolTraceStage::Lookup, true, "tool resolved"));

        let metadata = risk_metadata(tool.as_ref(), tool.name(), &call.input);
        let tool_context = ToolCtx {
            cwd: context.cwd,
            options: context.options,
            cancel: context.cancel,
            task_scope: context.task_scope,
            subagent: context.subagent,
            question: context.question,
            background_registry: context.background_registry.clone(),
        };

        if context.cancel.is_cancelled() {
            self.hooks
                .post_tool_use(&call.name, &call.input, false)
                .await;
            return cancelled_result(call, metadata, trace);
        }

        match tool.validate_input(&call.input, &tool_context).await {
            ValidationResult::Ok => {
                trace.push(trace_record(ToolTraceStage::Validate, true, "input valid"))
            }
            ValidationResult::Invalid { message, .. } => {
                trace.push(trace_record(
                    ToolTraceStage::Validate,
                    false,
                    message.clone(),
                ));
                self.hooks
                    .post_tool_use(&call.name, &call.input, false)
                    .await;
                return failed_result(
                    call,
                    metadata,
                    trace,
                    format!("Validation error: {message}"),
                );
            }
        }

        let permission_started = Instant::now();
        let tool_decision = tool.check_permissions(&call.input, &tool_context).await;
        let mut decision = self
            .gate
            .decide(&call.name, metadata.read_only, &tool_decision);
        if let PermissionDecision::Ask { message } = &decision {
            trace.push(trace_record(
                ToolTraceStage::PermissionRequest,
                true,
                if context.is_non_interactive {
                    "permission decision required in headless mode"
                } else {
                    "waiting for interactive permission decision"
                },
            ));
            if context.is_non_interactive {
                decision = self.gate.headless_resolve(decision);
            } else if let Some(resolver) = &self.permission_resolver {
                decision = resolver
                    .resolve(ToolPermissionRequest {
                        tool_use_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        input: call.input.clone(),
                        message: message.clone(),
                    })
                    .await;
            }
        }

        let mut effective_input = match decision {
            PermissionDecision::Allow { updated_input } => {
                trace.push(trace_record_elapsed(
                    ToolTraceStage::Permission,
                    true,
                    "permission allowed",
                    permission_started,
                ));
                updated_input.unwrap_or_else(|| call.input.clone())
            }
            PermissionDecision::Deny { reason } => {
                trace.push(trace_record_elapsed(
                    ToolTraceStage::Permission,
                    false,
                    reason.clone(),
                    permission_started,
                ));
                self.hooks
                    .post_tool_use(&call.name, &call.input, false)
                    .await;
                return failed_result(
                    call,
                    metadata,
                    trace,
                    format!("Permission denied: {reason}"),
                );
            }
            PermissionDecision::Ask { message } => {
                trace.push(trace_record_elapsed(
                    ToolTraceStage::Permission,
                    false,
                    "permission unresolved",
                    permission_started,
                ));
                self.hooks
                    .post_tool_use(&call.name, &call.input, false)
                    .await;
                return failed_result(
                    call,
                    metadata,
                    trace,
                    format!("Permission required (not granted): {message}"),
                );
            }
        };

        let mut pre_decision = self.hooks.pre_tool_use(&call.name, &effective_input).await;
        if let PermissionDecision::Ask { message } = &pre_decision {
            if context.is_non_interactive {
                pre_decision = self.gate.headless_resolve(pre_decision);
            } else if let Some(resolver) = &self.permission_resolver {
                pre_decision = resolver
                    .resolve(ToolPermissionRequest {
                        tool_use_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        input: effective_input.clone(),
                        message: message.clone(),
                    })
                    .await;
            }
        }
        match pre_decision {
            PermissionDecision::Allow { updated_input } => {
                if let Some(input) = updated_input {
                    effective_input = input;
                }
                trace.push(trace_record(
                    ToolTraceStage::PreHook,
                    true,
                    "pre-tool hooks allowed",
                ));
            }
            PermissionDecision::Deny { reason } => {
                trace.push(trace_record(
                    ToolTraceStage::PreHook,
                    false,
                    "pre-tool hook denied",
                ));
                self.hooks
                    .post_tool_use(&call.name, &effective_input, false)
                    .await;
                return failed_result(
                    call,
                    metadata,
                    trace,
                    format!("Permission denied by PreToolUse hook: {reason}"),
                );
            }
            PermissionDecision::Ask { message } => {
                trace.push(trace_record(
                    ToolTraceStage::PreHook,
                    false,
                    "pre-tool hook ask unresolved",
                ));
                self.hooks
                    .post_tool_use(&call.name, &effective_input, false)
                    .await;
                return failed_result(
                    call,
                    metadata,
                    trace,
                    format!("Permission required by PreToolUse hook: {message}"),
                );
            }
        }

        let call_started = Instant::now();
        let invocation = tokio::select! {
            biased;
            _ = context.cancel.cancelled() => Err(nonoclaw_core::Error::Cancelled),
            result = tool.call(effective_input.clone(), &tool_context, context.cancel.child_token()) => result,
        };
        let (raw_content, is_error, task_changes) = match invocation {
            Ok(result) => {
                trace.push(trace_record_elapsed(
                    ToolTraceStage::Call,
                    true,
                    "call succeeded",
                    call_started,
                ));
                (result.data, false, result.task_changes)
            }
            Err(error) => {
                trace.push(trace_record_elapsed(
                    ToolTraceStage::Call,
                    false,
                    error.to_string(),
                    call_started,
                ));
                (format!("Error: {error}"), true, Vec::new())
            }
        };

        self.hooks
            .post_tool_use(&call.name, &effective_input, !is_error)
            .await;
        trace.push(trace_record(
            ToolTraceStage::PostHook,
            true,
            if is_error {
                "failure hooks completed"
            } else {
                "success hooks completed"
            },
        ));

        let normalized = normalize_result(
            context.cwd,
            &call.id,
            &call.name,
            raw_content,
            tool.max_result_size_chars(),
        )
        .await;
        trace.push(trace_record(
            ToolTraceStage::Normalize,
            normalized.persist_error.is_none(),
            normalized.persist_error.clone().unwrap_or_else(|| {
                if normalized.local_reference.is_some() {
                    "oversized result summarized and saved".into()
                } else {
                    "result within inline limit".into()
                }
            }),
        ));

        ToolExecutionResult {
            id: call.id.clone(),
            name: call.name.clone(),
            content: normalized.content,
            is_error,
            task_changes,
            metadata: Some(metadata),
            trace,
            local_reference: normalized.local_reference,
            original_chars: normalized.original_chars,
        }
    }
}

#[derive(Debug)]
struct Batch {
    concurrency_safe: bool,
    indices: Vec<usize>,
}

fn partition_calls(calls: &[ToolCall], registry: &ToolRegistry) -> Vec<Batch> {
    let mut batches = Vec::new();
    let mut safe_indices = Vec::new();

    for (index, call) in calls.iter().enumerate() {
        let safe = registry
            .find(&call.name)
            .map(|tool| tool.is_concurrency_safe(&call.input))
            .unwrap_or(false);
        if safe {
            safe_indices.push(index);
        } else {
            if !safe_indices.is_empty() {
                batches.push(Batch {
                    concurrency_safe: true,
                    indices: std::mem::take(&mut safe_indices),
                });
            }
            batches.push(Batch {
                concurrency_safe: false,
                indices: vec![index],
            });
        }
    }
    if !safe_indices.is_empty() {
        batches.push(Batch {
            concurrency_safe: true,
            indices: safe_indices,
        });
    }
    batches
}

fn risk_metadata(tool: &dyn crate::tool::Tool, name: &str, input: &Value) -> ToolRiskMetadata {
    let read_only = tool.is_read_only(input);
    let destructive = tool.is_destructive(input);
    let paths = collect_string_fields(
        input,
        &[
            "file_path",
            "path",
            "directory",
            "root",
            "old_path",
            "new_path",
        ],
    );
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let network_targets = collect_string_fields(input, &["url", "endpoint", "server"]);
    let accesses_network = name.starts_with("Web")
        || name.starts_with("Mcp")
        || name.starts_with("mcp__")
        || !network_targets.is_empty();
    let writes_or_overwrites = destructive
        || matches!(name, "Write" | "Edit")
        || input.get("content").is_some() && !read_only;

    ToolRiskMetadata {
        read_only,
        concurrency_safe: tool.is_concurrency_safe(input),
        destructive,
        accesses_network,
        writes_or_overwrites,
        paths,
        command,
        network_targets,
    }
}

fn collect_string_fields(input: &Value, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter_map(|name| input.get(*name).and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

struct NormalizedResult {
    content: String,
    local_reference: Option<PathBuf>,
    original_chars: usize,
    persist_error: Option<String>,
}

async fn normalize_result(
    cwd: &Path,
    id: &str,
    tool_name: &str,
    raw: String,
    max_chars: usize,
) -> NormalizedResult {
    let original_chars = raw.chars().count();
    if original_chars <= max_chars {
        return NormalizedResult {
            content: raw,
            local_reference: None,
            original_chars,
            persist_error: None,
        };
    }

    let relative = PathBuf::from(".nonoclaw")
        .join("tool-results")
        .join(format!(
            "{}-{}.txt",
            sanitize_component(id),
            sanitize_component(tool_name)
        ));
    let absolute = cwd.join(&relative);
    let persisted = async {
        let parent = absolute.parent().expect("tool result path has a parent");
        tokio::fs::create_dir_all(parent).await?;
        tokio::fs::write(&absolute, raw.as_bytes()).await
    }
    .await;

    let body_limit = max_chars.saturating_sub(SUMMARY_SUFFIX_RESERVE).max(128);
    let head_chars = body_limit * 2 / 3;
    let tail_chars = body_limit.saturating_sub(head_chars);
    let head: String = raw.chars().take(head_chars).collect();
    let tail: String = raw
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    match persisted {
        Ok(()) => NormalizedResult {
            content: format!(
                "{head}\n\n…[{} characters omitted; full result saved locally]\n\n{tail}\n\n[Full result: {}]",
                original_chars.saturating_sub(head_chars + tail_chars),
                relative.display()
            ),
            local_reference: Some(relative),
            original_chars,
            persist_error: None,
        },
        Err(error) => NormalizedResult {
            content: format!(
                "{head}\n\n…[{} characters omitted; failed to save full result: {error}]\n\n{tail}",
                original_chars.saturating_sub(head_chars + tail_chars)
            ),
            local_reference: None,
            original_chars,
            persist_error: Some(format!("failed to persist oversized result: {error}")),
        },
    }
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    if sanitized.is_empty() {
        "tool-call".into()
    } else {
        sanitized
    }
}

fn trace_record(stage: ToolTraceStage, ok: bool, detail: impl Into<String>) -> ToolTraceRecord {
    ToolTraceRecord {
        stage,
        ok,
        detail: detail.into(),
        elapsed_ms: 0,
    }
}

fn trace_record_elapsed(
    stage: ToolTraceStage,
    ok: bool,
    detail: impl Into<String>,
    started: Instant,
) -> ToolTraceRecord {
    ToolTraceRecord {
        stage,
        ok,
        detail: detail.into(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

fn failed_result(
    call: &ToolCall,
    metadata: ToolRiskMetadata,
    trace: Vec<ToolTraceRecord>,
    content: String,
) -> ToolExecutionResult {
    let original_chars = content.chars().count();
    ToolExecutionResult {
        id: call.id.clone(),
        name: call.name.clone(),
        content,
        is_error: true,
        task_changes: Vec::new(),
        metadata: Some(metadata),
        trace,
        local_reference: None,
        original_chars,
    }
}

fn cancelled_result(
    call: &ToolCall,
    metadata: ToolRiskMetadata,
    mut trace: Vec<ToolTraceRecord>,
) -> ToolExecutionResult {
    trace.push(trace_record(
        ToolTraceStage::Call,
        false,
        "execution cancelled",
    ));
    failed_result(call, metadata, trace, "Error: operation cancelled".into())
}

fn parse_max_tool_concurrency(value: Option<&str>) -> usize {
    value
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CONCURRENCY)
        .max(1)
}

pub fn max_tool_concurrency_from_env() -> usize {
    parse_max_tool_concurrency(
        std::env::var("NONOCLAW_MAX_TOOL_CONCURRENCY")
            .ok()
            .as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolResult};
    use nonoclaw_core::{PermissionResult, Result};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct UpdatingHooks;

    #[async_trait]
    impl ToolHookRunner for UpdatingHooks {
        async fn pre_tool_use(&self, _: &str, _: &Value) -> PermissionDecision {
            PermissionDecision::Allow {
                updated_input: Some(json!({"label":"rewritten"})),
            }
        }
    }

    struct RecordingHooks(Arc<AtomicUsize>);

    #[async_trait]
    impl ToolHookRunner for RecordingHooks {
        async fn post_tool_use(&self, _: &str, _: &Value, success: bool) {
            if !success {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    struct ProbeTool {
        name: &'static str,
        safe: bool,
        delay_ms: u64,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        completions: Arc<Mutex<Vec<String>>>,
        result_chars: usize,
    }

    #[async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn prompt(&self) -> &str {
            "probe"
        }
        fn description(&self) -> &str {
            "probe"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        fn is_read_only(&self, _: &Value) -> bool {
            true
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            self.safe
        }
        fn max_result_size_chars(&self) -> usize {
            256
        }
        async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
            PermissionDecision::allow()
        }
        async fn call(
            &self,
            input: Value,
            _: &ToolCtx<'_>,
            _: CancellationToken,
        ) -> Result<ToolResult> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            let label = input["label"].as_str().unwrap_or("probe").to_string();
            self.completions.lock().unwrap().push(label.clone());
            self.active.fetch_sub(1, Ordering::SeqCst);
            let data = if self.result_chars > 0 {
                label.repeat(self.result_chars)
            } else {
                label
            };
            Ok(ToolResult::ok(data))
        }
    }

    type ProbeRegistryFixture = (
        Arc<ToolRegistry>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
    );

    fn probe_registry(safe_delay: u64, barrier_delay: u64) -> ProbeRegistryFixture {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let completions = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ProbeTool {
            name: "Safe",
            safe: true,
            delay_ms: safe_delay,
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
            completions: Arc::clone(&completions),
            result_chars: 0,
        }));
        registry.register(Arc::new(ProbeTool {
            name: "Barrier",
            safe: false,
            delay_ms: barrier_delay,
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
            completions: Arc::clone(&completions),
            result_chars: 0,
        }));
        (Arc::new(registry), active, peak, completions)
    }

    fn context<'a>(
        cwd: &'a Path,
        options: &'a ToolOptions,
        cancel: &'a CancellationToken,
    ) -> ToolExecutionContext<'a> {
        ToolExecutionContext {
            cwd,
            options,
            cancel,
            task_scope: Some("executor-test"),
            subagent: None,
            question: None,
            background_registry: None,
            is_non_interactive: true,
        }
    }

    fn executor(registry: Arc<ToolRegistry>, cap: usize) -> ToolExecutor {
        ToolExecutor::new(
            registry,
            PermissionGate::new(nonoclaw_core::PermissionMode::Default, vec![], vec![]),
            Arc::new(NoopToolHooks),
            None,
            cap,
        )
    }

    #[tokio::test]
    async fn safe_batches_are_bounded_and_results_keep_original_order() {
        let (registry, _, peak, _) = probe_registry(20, 1);
        let executor = executor(registry, 2);
        let calls = (0..7)
            .map(|index| ToolCall {
                id: format!("id-{index}"),
                name: "Safe".into(),
                input: json!({"label": index.to_string()}),
            })
            .collect::<Vec<_>>();
        let cwd = std::env::temp_dir();
        let options = ToolOptions {
            model: "test".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let results = executor
            .execute(&calls, &context(&cwd, &options, &cancel))
            .await;

        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(
            results
                .iter()
                .map(|result| result.id.clone())
                .collect::<Vec<_>>(),
            (0..7)
                .map(|index| format!("id-{index}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn unsafe_calls_are_barriers_between_safe_batches() {
        let (registry, _, _, completions) = probe_registry(15, 1);
        let executor = executor(registry, 4);
        let calls = vec![
            ToolCall {
                id: "1".into(),
                name: "Safe".into(),
                input: json!({"label":"before-a"}),
            },
            ToolCall {
                id: "2".into(),
                name: "Safe".into(),
                input: json!({"label":"before-b"}),
            },
            ToolCall {
                id: "3".into(),
                name: "Barrier".into(),
                input: json!({"label":"barrier"}),
            },
            ToolCall {
                id: "4".into(),
                name: "Safe".into(),
                input: json!({"label":"after"}),
            },
        ];
        let cwd = std::env::temp_dir();
        let options = ToolOptions {
            model: "test".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        executor
            .execute(&calls, &context(&cwd, &options, &cancel))
            .await;

        let completed = completions.lock().unwrap().clone();
        let barrier = completed.iter().position(|item| item == "barrier").unwrap();
        let after = completed.iter().position(|item| item == "after").unwrap();
        assert!(completed[..barrier]
            .iter()
            .all(|item| item.starts_with("before-")));
        assert_eq!(barrier, 2);
        assert!(after > barrier);
    }

    #[tokio::test]
    async fn oversized_result_is_summarized_and_full_content_is_saved() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let completions = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ProbeTool {
            name: "Large",
            safe: true,
            delay_ms: 0,
            active,
            peak,
            completions,
            result_chars: 400,
        }));
        let executor = executor(Arc::new(registry), 1);
        let cwd =
            std::env::temp_dir().join(format!("nonoclaw-tool-result-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let options = ToolOptions {
            model: "test".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let calls = vec![ToolCall {
            id: "large/one".into(),
            name: "Large".into(),
            input: json!({"label":"x"}),
        }];
        let result = executor
            .execute(&calls, &context(&cwd, &options, &cancel))
            .await
            .remove(0);

        let reference = result.local_reference.expect("large result reference");
        assert!(result.content.contains("full result saved locally"));
        assert_eq!(
            tokio::fs::read_to_string(cwd.join(reference))
                .await
                .unwrap()
                .chars()
                .count(),
            400
        );
        tokio::fs::remove_dir_all(cwd).await.ok();
    }

    #[test]
    fn concurrency_parser_is_total_and_never_returns_zero() {
        // Property-style coverage over malformed, zero, and a broad valid range.
        for value in [None, Some(""), Some("nope"), Some("-1"), Some("0")] {
            assert!(parse_max_tool_concurrency(value) >= 1);
        }
        for cap in 1..=1024usize {
            let text = cap.to_string();
            assert_eq!(parse_max_tool_concurrency(Some(&text)), cap);
        }
    }

    #[test]
    fn partitioning_preserves_every_index_and_isolates_barriers() {
        // **Validates: Requirements 3.2**
        let (registry, _, _, _) = probe_registry(0, 0);
        for mask in 0_u16..256 {
            let calls = (0..8)
                .map(|index| ToolCall {
                    id: index.to_string(),
                    name: if mask & (1 << index) == 0 {
                        "Safe".into()
                    } else {
                        "Barrier".into()
                    },
                    input: json!({}),
                })
                .collect::<Vec<_>>();
            let batches = partition_calls(&calls, &registry);
            let flattened = batches
                .iter()
                .flat_map(|batch| batch.indices.iter().copied())
                .collect::<Vec<_>>();
            assert_eq!(flattened, (0..8).collect::<Vec<_>>());
            for batch in batches {
                if !batch.concurrency_safe {
                    assert_eq!(batch.indices.len(), 1);
                    assert_eq!(calls[batch.indices[0]].name, "Barrier");
                }
            }
        }
    }

    #[tokio::test]
    async fn pipeline_emits_all_stages_and_shared_risk_metadata() {
        // **Validates: Requirements 4.1, 4.2, 4.7**
        let (registry, _, _, _) = probe_registry(0, 0);
        let executor = executor(registry, 1);
        let cwd = std::env::temp_dir();
        let options = ToolOptions {
            model: "test".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let calls = vec![ToolCall {
            id: "trace".into(),
            name: "Safe".into(),
            input: json!({
                "label": "ok",
                "file_path": "/tmp/input.txt",
                "command": "git status",
                "url": "https://example.invalid"
            }),
        }];
        let result = executor
            .execute(&calls, &context(&cwd, &options, &cancel))
            .await
            .remove(0);

        assert!(!result.is_error);
        assert_eq!(
            result
                .trace
                .iter()
                .map(|record| record.stage)
                .collect::<Vec<_>>(),
            vec![
                ToolTraceStage::Lookup,
                ToolTraceStage::Validate,
                ToolTraceStage::Permission,
                ToolTraceStage::PreHook,
                ToolTraceStage::Call,
                ToolTraceStage::PostHook,
                ToolTraceStage::Normalize,
            ]
        );
        let metadata = result.metadata.unwrap();
        assert!(metadata.read_only);
        assert!(metadata.concurrency_safe);
        assert!(metadata.accesses_network);
        assert_eq!(metadata.paths, vec!["/tmp/input.txt"]);
        assert_eq!(metadata.command.as_deref(), Some("git status"));
        assert_eq!(metadata.network_targets, vec!["https://example.invalid"]);
    }

    #[tokio::test]
    async fn failure_hook_runs_for_unresolved_tool_calls() {
        // **Validates: Requirements 7.4**
        let failures = Arc::new(AtomicUsize::new(0));
        let executor = ToolExecutor::new(
            Arc::new(ToolRegistry::new()),
            PermissionGate::new(nonoclaw_core::PermissionMode::Default, vec![], vec![]),
            Arc::new(RecordingHooks(Arc::clone(&failures))),
            None,
            1,
        );
        let cwd = std::env::temp_dir();
        let options = ToolOptions {
            model: "test".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let result = executor
            .execute(
                &[ToolCall {
                    id: "missing".into(),
                    name: "Missing".into(),
                    input: json!({}),
                }],
                &context(&cwd, &options, &cancel),
            )
            .await
            .remove(0);
        assert!(result.is_error);
        assert_eq!(failures.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pre_hook_updated_input_reaches_tool_call() {
        // **Validates: Requirements 7.5**
        let (registry, _, _, _) = probe_registry(0, 0);
        let executor = ToolExecutor::new(
            registry,
            PermissionGate::new(nonoclaw_core::PermissionMode::Default, vec![], vec![]),
            Arc::new(UpdatingHooks),
            None,
            1,
        );
        let cwd = std::env::temp_dir();
        let options = ToolOptions {
            model: "test".into(),
            permission_mode: nonoclaw_core::PermissionMode::Default,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let result = executor
            .execute(
                &[ToolCall {
                    id: "updated".into(),
                    name: "Safe".into(),
                    input: json!({"label":"original"}),
                }],
                &context(&cwd, &options, &cancel),
            )
            .await
            .remove(0);

        assert!(!result.is_error);
        assert_eq!(result.content, "rewritten");
    }

    #[test]
    fn sanitization_never_emits_path_separators() {
        // **Validates: Requirements 4.3, 4.7**
        for input in ["../escape", "a/b\\c", "", "合法-id", "id with spaces"] {
            let output = sanitize_component(input);
            assert!(!output.contains('/'));
            assert!(!output.contains('\\'));
            assert!(!output.is_empty());
        }
    }
}
