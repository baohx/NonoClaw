//! In-memory collector and redacted JSON export for one canonical run stream.

use std::path::Path;
use std::sync::{Arc, Mutex};

use nonoclaw_core::{EventEnvelope, RunEvent};

#[derive(Debug, Clone, Default)]
pub struct TraceCollector {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
}

impl TraceCollector {
    pub fn record(&self, event: EventEnvelope) {
        self.events.lock().unwrap().push(event);
    }

    pub fn snapshot(&self) -> Vec<EventEnvelope> {
        self.events.lock().unwrap().clone()
    }

    pub fn export_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(&self.snapshot())
    }

    pub fn export_path(&self, path: &Path) -> std::io::Result<()> {
        let json = self
            .export_json()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        std::fs::write(path, json)
    }

    /// Ledger-relevant timing subset of the recorded stream, newest-capped to
    /// `max_events`. Mirrors the frontend `isTimingBoundary` predicate: window
    /// opens/closes (model request, stream state, thinking end, first token),
    /// tool execution spans, usage, retries and terminal facts. Content-heavy
    /// deltas are dropped — only timing-relevant envelopes survive.
    pub fn timing_boundary_snapshot(&self, max_events: usize) -> Vec<EventEnvelope> {
        let mut kept: Vec<EventEnvelope> = self
            .snapshot()
            .into_iter()
            .filter(|envelope| is_timing_boundary(&envelope.event))
            .collect();
        if kept.len() > max_events {
            let drop = kept.len() - max_events;
            kept.drain(..drop);
        }
        kept
    }

    /// Return the complete, low-frequency event stream needed to reproduce
    /// the browser's trajectory views after a reload. Child contexts share
    /// this collector, but the live browser receives child activity only as a
    /// scoped `SubagentEvent`; raw child envelopes are therefore excluded so
    /// replay remains equivalent to the live stream.
    pub fn replay_snapshot(&self, root_run_id: &str) -> Vec<EventEnvelope> {
        self.snapshot()
            .into_iter()
            .filter(|envelope| envelope.run_id == root_run_id && is_replay_event(&envelope.event))
            .collect()
    }
}

fn is_replay_event(event: &RunEvent) -> bool {
    match event {
        // These are high-frequency content streams. Messages remain the
        // canonical persisted source for visible assistant/user content.
        RunEvent::TextDelta { .. } | RunEvent::ThinkingDelta { .. } => false,
        // The engine may emit active=true before every reasoning delta; the
        // first/last durable timing facts are carried by stream state and the
        // active=false close transition.
        RunEvent::ThinkingState { active: true, .. } => false,
        // Nested wrappers are never valid. For a scoped child retain all
        // bounded technical/tool/lifecycle facts, but not content deltas.
        RunEvent::SubagentEvent { event, .. } => match event.as_ref() {
            RunEvent::TextDelta { .. }
            | RunEvent::ThinkingDelta { .. }
            | RunEvent::ThinkingState { active: true, .. }
            | RunEvent::SubagentEvent { .. } => false,
            _ => true,
        },
        _ => true,
    }
}

fn is_timing_boundary(event: &RunEvent) -> bool {
    match event {
        RunEvent::ModelRequestStarted { .. }
        | RunEvent::UsageUpdated { .. }
        | RunEvent::ToolUseStart { .. }
        | RunEvent::ToolExecutionStarted { .. }
        | RunEvent::ToolExecutionFinished { .. }
        | RunEvent::RunFinished { .. }
        | RunEvent::RetryScheduled { .. } => true,
        RunEvent::ThinkingState { active: false, .. } => true,
        RunEvent::StreamStateChanged { state, .. } => matches!(
            state,
            nonoclaw_core::StreamState::Streaming
                | nonoclaw_core::StreamState::Completed
                | nonoclaw_core::StreamState::Interrupted
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_core::{RunEvent, TechnicalStatus};

    #[test]
    fn export_preserves_order_and_uses_redacted_envelopes() {
        // **Validates: Requirements 9.1, 9.7, 9.8**
        let collector = TraceCollector::default();
        for sequence in 1..=3 {
            collector.record(EventEnvelope::at(
                "run",
                None,
                "session",
                0,
                sequence,
                sequence,
                RunEvent::RunError {
                    code: "fixture".into(),
                    operation: "test".into(),
                    retryable: false,
                    message: "authorization: Bearer secret".into(),
                },
            ));
        }
        let value: serde_json::Value =
            serde_json::from_str(&collector.export_json().unwrap()).unwrap();
        assert_eq!(value[0]["sequence"], 1);
        assert_eq!(value[2]["sequence"], 3);
        assert_eq!(value[0]["event"]["message"], "[REDACTED]");
        let _ = TechnicalStatus::Succeeded;
    }

    #[test]
    fn timing_boundary_snapshot_keeps_only_window_facts() {
        let collector = TraceCollector::default();
        let mut sequence = 0;
        let mut record = |event: RunEvent| {
            sequence += 1;
            collector.record(EventEnvelope::at(
                "run",
                None,
                "session",
                0,
                sequence,
                sequence * 100,
                event,
            ));
        };
        // Content-heavy deltas must drop; window opens/closes must stay.
        record(RunEvent::TextDelta {
            text: "answer".into(),
        });
        record(RunEvent::ThinkingDelta {
            text: "reasoning".into(),
            turn: 1,
        });
        record(RunEvent::ModelRequestStarted {
            requested_model: "m".into(),
            provider: "p".into(),
            turn: 1,
        });
        record(RunEvent::StreamStateChanged {
            state: nonoclaw_core::StreamState::Streaming,
            turn: 1,
        });
        record(RunEvent::ThinkingState {
            active: true,
            turn: 1,
        });
        record(RunEvent::ThinkingState {
            active: false,
            turn: 1,
        });
        record(RunEvent::UsageUpdated {
            turn: 1,
            turn_usage: nonoclaw_core::UsagePart::default(),
            total: nonoclaw_core::Usage::default(),
            max_budget_usd: None,
        });

        let kept = collector.timing_boundary_snapshot(64);
        let kinds: Vec<String> = kept
            .iter()
            .map(|envelope| {
                serde_json::to_value(&envelope.event)
                    .unwrap()
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "model_request_started",
                "stream_state_changed",
                "thinking_state",
                "usage_updated",
            ],
            "deltas drop; boundaries stay in order"
        );
        assert!(kept.iter().all(|e| e.run_id == "run"));

        // The cap keeps the newest window facts, not the oldest.
        let capped = collector.timing_boundary_snapshot(2);
        assert_eq!(capped.len(), 2);
        let last_kind = serde_json::to_value(&capped[1].event)
            .unwrap()
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap()
            .to_string();
        assert_eq!(last_kind, "usage_updated", "newest events survive the cap");
    }
}
