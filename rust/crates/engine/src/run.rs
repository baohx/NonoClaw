//! Canonical lifecycle ownership for one agent run.
//!
//! Every entry point (headless, Web, remote, and child agents) starts work
//! through [`RunController`]. The controller owns the root cancellation token,
//! the ordered event relay, the top-level task, and the exactly-once terminal
//! commit.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use nonoclaw_core::{
    AppError, Error, EventEnvelope, MessageContent, RunEvent, RunId, TechnicalStatus,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::loop_::{FinalResult, QueryEngine, RunFinishReason};
use crate::trace::TraceCollector;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunLimits {
    pub max_turns: u32,
    pub max_budget_usd: Option<f64>,
    pub context_window: Option<usize>,
}

/// Immutable identity and shared cancellation/ordering state for one run.
#[derive(Clone)]
pub struct RunContext {
    pub run_id: RunId,
    pub parent_run_id: Option<RunId>,
    pub session_id: String,
    pub cwd: PathBuf,
    pub model: String,
    pub limits: RunLimits,
    pub cancel: CancellationToken,
    pub trace: TraceCollector,
    sequence: Arc<AtomicU64>,
    cancel_reason: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for RunContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunContext")
            .field("run_id", &self.run_id)
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("model", &self.model)
            .field("limits", &self.limits)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

impl RunContext {
    pub fn new(
        session_id: impl Into<String>,
        cwd: PathBuf,
        model: impl Into<String>,
        limits: RunLimits,
    ) -> Self {
        Self::with_cancel(
            session_id,
            cwd,
            model,
            limits,
            CancellationToken::new(),
            None,
        )
    }

    fn with_cancel(
        session_id: impl Into<String>,
        cwd: PathBuf,
        model: impl Into<String>,
        limits: RunLimits,
        cancel: CancellationToken,
        parent_run_id: Option<RunId>,
    ) -> Self {
        Self {
            run_id: uuid::Uuid::new_v4().to_string(),
            parent_run_id,
            session_id: session_id.into(),
            cwd,
            model: model.into(),
            limits,
            cancel,
            trace: TraceCollector::default(),
            sequence: Arc::new(AtomicU64::new(0)),
            cancel_reason: Arc::new(Mutex::new(None)),
        }
    }

    /// Create an independently identified child run whose cancellation token is
    /// rooted in this run. Cancelling the parent therefore cancels every child,
    /// while a child can finish/cancel without affecting its siblings.
    pub fn child(
        &self,
        session_id: impl Into<String>,
        cwd: PathBuf,
        model: impl Into<String>,
        limits: RunLimits,
    ) -> Self {
        let mut child = Self::with_cancel(
            session_id,
            cwd,
            model,
            limits,
            self.cancel.child_token(),
            Some(self.run_id.clone()),
        );
        child.trace = self.trace.clone();
        child
    }

    pub fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn envelope(&self, event: RunEvent) -> EventEnvelope {
        EventEnvelope::new(
            self.run_id.clone(),
            self.parent_run_id.clone(),
            self.session_id.clone(),
            0,
            self.next_sequence(),
            event,
        )
    }

    pub fn cancel(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let mut current = self.cancel_reason.lock().unwrap();
        if current.is_none() {
            *current = Some(reason);
        }
        drop(current);
        self.cancel.cancel();
    }

    pub fn cancellation_reason(&self) -> Option<String> {
        self.cancel_reason.lock().unwrap().clone()
    }
}

/// Backward-compatible name for the canonical core envelope.
pub type SequencedEngineEvent = EventEnvelope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTerminalStatus {
    Done,
    Cancelled,
    Error,
}

/// The one authoritative terminal record for a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunTerminal {
    pub run_id: RunId,
    pub session_id: String,
    pub sequence: u64,
    pub status: RunTerminalStatus,
    pub reason: RunFinishReason,
    pub result: Option<FinalResult>,
}

#[derive(Clone)]
pub struct RunController {
    context: RunContext,
    terminal: Arc<OnceLock<RunTerminal>>,
    started: Arc<AtomicBool>,
}

impl RunController {
    pub fn new(context: RunContext) -> Self {
        Self {
            context,
            terminal: Arc::new(OnceLock::new()),
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn for_engine(engine: &QueryEngine, cwd: PathBuf) -> Self {
        Self::new(engine.run_context(cwd))
    }

    pub fn context(&self) -> &RunContext {
        &self.context
    }

    pub fn cancel(&self, reason: impl Into<String>) {
        self.context.cancel(reason);
    }

    pub fn terminal(&self) -> Option<&RunTerminal> {
        self.terminal.get()
    }

    fn commit(
        &self,
        status: RunTerminalStatus,
        reason: RunFinishReason,
        result: Option<FinalResult>,
    ) -> RunTerminal {
        let sequence = self.context.next_sequence();
        if !self
            .context
            .trace
            .snapshot()
            .last()
            .is_some_and(|event| matches!(&event.event, RunEvent::RunFinished { .. }))
        {
            self.context.trace.record(EventEnvelope::new(
                self.context.run_id.clone(),
                self.context.parent_run_id.clone(),
                self.context.session_id.clone(),
                0,
                sequence,
                RunEvent::RunFinished {
                    status: match status {
                        RunTerminalStatus::Done => TechnicalStatus::Succeeded,
                        RunTerminalStatus::Cancelled => TechnicalStatus::Cancelled,
                        RunTerminalStatus::Error => TechnicalStatus::Failed,
                    },
                    reason: format!("{reason:?}"),
                    duration_ms: 0,
                    turns: result.as_ref().map(|value| value.turns).unwrap_or(0),
                    usage: result.as_ref().map(|value| value.usage).unwrap_or_default(),
                },
            ));
        }
        let candidate = RunTerminal {
            run_id: self.context.run_id.clone(),
            session_id: self.context.session_id.clone(),
            sequence,
            status,
            reason,
            result,
        };
        let _ = self.terminal.set(candidate);
        self.terminal
            .get()
            .expect("terminal set by this or a concurrent committer")
            .clone()
    }

    /// Spawn the engine and its ordered event consumer under this controller.
    /// The returned handle is the sole owner of the top-level supervisor task.
    pub fn start<F, Fut>(
        &self,
        mut engine: QueryEngine,
        user_content: MessageContent,
        mut consume: F,
    ) -> RunHandle
    where
        F: FnMut(SequencedEngineEvent) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        assert!(
            !self.started.swap(true, Ordering::SeqCst),
            "RunController can start only one top-level run"
        );

        let controller = self.clone();
        let context = self.context.clone();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let consumer = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                consume(event).await;
            }
        });

        let engine_context = context.clone();
        let engine_task = tokio::spawn(async move {
            let started = Instant::now();
            let result = engine
                .run_with_context(user_content, &engine_context, |event| {
                    let envelope = engine_context.envelope(event.clone());
                    engine_context.trace.record(envelope.clone());
                    let _ = event_tx.send(envelope);
                })
                .await;
            if let Err(error) = &result {
                let cancelled =
                    engine_context.cancel.is_cancelled() || matches!(error, Error::Cancelled);
                let mut terminal_events = Vec::new();
                if cancelled {
                    terminal_events.push(RunEvent::CancellationRequested {
                        reason: engine_context
                            .cancellation_reason()
                            .unwrap_or_else(|| "run cancellation requested".into()),
                    });
                } else {
                    let safe = AppError::from_core(error, "run");
                    terminal_events.push(RunEvent::RunError {
                        code: format!("{:?}", safe.code).to_lowercase(),
                        operation: safe.operation.clone(),
                        retryable: safe.retryable,
                        message: safe.message.clone(),
                    });
                }
                let safe_reason = if cancelled {
                    "run cancelled".to_string()
                } else {
                    AppError::from_core(error, "run").message
                };
                terminal_events.push(RunEvent::RunFinished {
                    status: if cancelled {
                        TechnicalStatus::Cancelled
                    } else {
                        TechnicalStatus::Failed
                    },
                    reason: safe_reason,
                    duration_ms: started.elapsed().as_millis() as u64,
                    turns: 0,
                    usage: Default::default(),
                });
                for event in terminal_events {
                    let envelope = engine_context.envelope(event);
                    engine_context.trace.record(envelope.clone());
                    let _ = event_tx.send(envelope);
                }
            }
            (engine, result)
        });

        let supervisor = tokio::spawn(async move {
            let engine_join = engine_task.await;
            let consumer_join = consumer.await;

            if let Err(_consumer_error) = consumer_join {
                let engine = match engine_join {
                    Ok((engine, _)) => Some(engine),
                    Err(_) => None,
                };
                let terminal = controller.commit(
                    RunTerminalStatus::Error,
                    RunFinishReason::Error {
                        message: "event delivery failed".into(),
                    },
                    None,
                );
                return RunCompletion { engine, terminal };
            }

            match engine_join {
                Ok((engine, Ok(result))) => {
                    let reason = result.finish_reason.clone();
                    let terminal = controller.commit(RunTerminalStatus::Done, reason, Some(result));
                    RunCompletion {
                        engine: Some(engine),
                        terminal,
                    }
                }
                Ok((engine, Err(error))) => {
                    let cancelled =
                        context.cancel.is_cancelled() || matches!(error, Error::Cancelled);
                    let (status, reason) = if cancelled {
                        (
                            RunTerminalStatus::Cancelled,
                            RunFinishReason::Cancelled {
                                reason: context
                                    .cancellation_reason()
                                    .unwrap_or_else(|| "run cancellation requested".into()),
                            },
                        )
                    } else {
                        (
                            RunTerminalStatus::Error,
                            RunFinishReason::Error {
                                message: AppError::from_core(&error, "run").message,
                            },
                        )
                    };
                    let terminal = controller.commit(status, reason, None);
                    RunCompletion {
                        engine: Some(engine),
                        terminal,
                    }
                }
                Err(_join_error) => {
                    let reason = RunFinishReason::Error {
                        message: "run task failed".into(),
                    };
                    let terminal = controller.commit(RunTerminalStatus::Error, reason, None);
                    RunCompletion {
                        engine: None,
                        terminal,
                    }
                }
            }
        });

        RunHandle {
            controller: self.clone(),
            supervisor,
        }
    }
}

pub struct RunCompletion {
    pub engine: Option<QueryEngine>,
    pub terminal: RunTerminal,
}

pub struct RunHandle {
    controller: RunController,
    supervisor: JoinHandle<RunCompletion>,
}

impl RunHandle {
    pub fn controller(&self) -> &RunController {
        &self.controller
    }

    pub fn cancel(&self, reason: impl Into<String>) {
        self.controller.cancel(reason);
    }

    pub async fn wait(self) -> RunCompletion {
        match self.supervisor.await {
            Ok(completion) => completion,
            Err(_error) => {
                let terminal = self.controller.commit(
                    RunTerminalStatus::Error,
                    RunFinishReason::Error {
                        message: "run supervisor failed".into(),
                    },
                    None,
                );
                RunCompletion {
                    engine: None,
                    terminal,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_have_unique_ids_and_parent_cancellation_reaches_children() {
        let parent = RunContext::new(
            "session-parent",
            PathBuf::from("/tmp"),
            "model",
            RunLimits::default(),
        );
        let child = parent.child(
            "session-child",
            PathBuf::from("/tmp"),
            "model",
            RunLimits::default(),
        );
        assert_ne!(parent.run_id, child.run_id);
        assert_eq!(parent.session_id, "session-parent");
        assert_eq!(child.session_id, "session-child");
        parent.cancel("user requested cancellation");
        assert!(parent.cancel.is_cancelled());
        assert!(child.cancel.is_cancelled());
        assert_eq!(
            parent.cancellation_reason().as_deref(),
            Some("user requested cancellation")
        );
    }

    #[tokio::test]
    async fn event_sequence_is_unique_under_concurrency() {
        let context = RunContext::new(
            "session",
            PathBuf::from("/tmp"),
            "model",
            RunLimits::default(),
        );
        let mut tasks = Vec::new();
        for _ in 0..64 {
            let context = context.clone();
            tasks.push(tokio::spawn(async move { context.next_sequence() }));
        }
        let mut values = Vec::new();
        for task in tasks {
            values.push(task.await.unwrap());
        }
        values.sort_unstable();
        assert_eq!(values, (1..=64).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn cancelling_controller_stops_inflight_provider_and_commits_cancelled_once() {
        use std::sync::Arc;
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _stream = stream;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        let client = Arc::new(
            nonoclaw_api::Client::new(Some("fixture-key".into()), None, format!("http://{addr}"))
                .unwrap(),
        );
        let (registry, todos) = nonoclaw_tools::register_all();
        let options = crate::loop_::EngineOptions {
            model: "fixture-model".into(),
            max_turns: 1,
            auto_compact: false,
            ..Default::default()
        };
        let engine = QueryEngine::new(client, Arc::new(registry), todos, options);
        let cwd =
            std::env::temp_dir().join(format!("nonoclaw-run-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let controller = RunController::for_engine(&engine, cwd);
        let events = Arc::new(Mutex::new(Vec::<EventEnvelope>::new()));
        let captured = Arc::clone(&events);
        let handle = controller.start(
            engine,
            MessageContent::from_text("wait for cancellation"),
            move |event| {
                let captured = Arc::clone(&captured);
                async move {
                    captured.lock().unwrap().push(event);
                }
            },
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.cancel("test cancellation");
        let completion = tokio::time::timeout(Duration::from_secs(3), handle.wait())
            .await
            .expect("cancelled run must finish promptly");
        fixture.abort();

        assert_eq!(completion.terminal.status, RunTerminalStatus::Cancelled);
        assert!(matches!(
            completion.terminal.reason,
            RunFinishReason::Cancelled { ref reason } if reason == "test cancellation"
        ));
        assert_eq!(
            controller.terminal().unwrap().sequence,
            completion.terminal.sequence
        );
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.event,
            RunEvent::CancellationRequested { reason } if reason == "test cancellation"
        )));
        assert!(matches!(
            events.last().map(|event| &event.event),
            Some(RunEvent::RunFinished {
                status: TechnicalStatus::Cancelled,
                ..
            })
        ));
        assert!(events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
    }

    #[tokio::test]
    async fn terminal_commit_is_exactly_once_during_cancel_error_race() {
        let controller = RunController::new(RunContext::new(
            "session",
            PathBuf::from("/tmp"),
            "model",
            RunLimits::default(),
        ));
        let a = controller.clone();
        let b = controller.clone();
        let first = tokio::spawn(async move {
            a.commit(
                RunTerminalStatus::Cancelled,
                RunFinishReason::Cancelled {
                    reason: "cancel".into(),
                },
                None,
            )
        });
        let second = tokio::spawn(async move {
            b.commit(
                RunTerminalStatus::Error,
                RunFinishReason::Error {
                    message: "late error".into(),
                },
                None,
            )
        });
        let left = first.await.unwrap();
        let right = second.await.unwrap();
        assert_eq!(left.sequence, right.sequence);
        assert_eq!(left.status, right.status);
        assert_eq!(controller.terminal().unwrap().sequence, left.sequence);
    }
}
