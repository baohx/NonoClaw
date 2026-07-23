//! In-memory collector and redacted JSON export for one canonical run stream.

use std::path::Path;
use std::sync::{Arc, Mutex};

use nonoclaw_core::EventEnvelope;

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
}
