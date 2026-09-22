//! Jev (TypeSafe AI "System One") client.
//!
//! Jev is not a chat model: it evaluates a `state` against typed questions
//! (`choice` / `score` / `noul`) and returns calibrated probabilities. This
//! client is deliberately minimal — a single POST to `/v1/systemone` with a
//! short timeout. Every caller must be able to proceed without Jev: network
//! failure, missing key, or disabled mode degrade to `None`, never panic or
//! block the engine loop.
//!
//! The engine depends on this via a trait (`JevDecision`) so tests can stub
//! it without HTTP.

use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default API endpoint. Configurable for enterprise/self-hosted gateways.
pub const JEV_DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai";

/// Minimum calibrated confidence for a Jev answer to be trusted. Below this,
/// callers keep their heuristic outcome (fail-open to current behavior).
pub const JEV_MIN_CONFIDENCE: f64 = 0.6;

/// Hard ceiling for a Jev call. Jev answers in milliseconds; anything slower
/// is a network/proxy stall and the caller should fall back immediately.
const JEV_TIMEOUT: Duration = Duration::from_secs(5);

/// Error type for Jev calls. Always non-fatal by contract: callers map this
/// to "use the heuristic path instead".
#[derive(Debug, thiserror::Error)]
pub enum JevError {
    #[error("jev: request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("jev: unexpected response shape: {0}")]
    Parse(String),
}

/// One question in a Jev request. Only the variants NonoClaw needs.
///
/// Wire shape (verified against the live API 2026-09-21): every question is
/// `{ type, instructions, criteria? }` — `criteria` is the options map for
/// Choice (required), the ordered levels for Score, and an optional yes/no
/// clarification for Noul.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum JevQuestion {
    /// Pick one option from a fixed choice set; returns the full
    /// probability distribution.
    Choice {
        instructions: String,
        criteria: std::collections::BTreeMap<String, String>,
    },
    /// Yes/no judgment (Jev's "Noul" primitive).
    Noul { instructions: String },
}

impl JevQuestion {
    pub fn choice(instructions: impl Into<String>, options: &[(&str, &str)]) -> Self {
        JevQuestion::Choice {
            instructions: instructions.into(),
            criteria: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    pub fn noul(instructions: impl Into<String>) -> Self {
        JevQuestion::Noul {
            instructions: instructions.into(),
        }
    }
}

/// Parsed answer for one question.
#[derive(Debug, Clone, Deserialize)]
pub struct JevAnswer {
    pub choice: Option<String>,
    #[serde(default)]
    pub probabilities: std::collections::BTreeMap<String, f64>,
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Noul-style yes/no.
    #[serde(default)]
    pub answer: Option<serde_json::Value>,
    /// Noul probability: P(yes). The live API returns `{"type":"noul",
    /// "noul":0.1}` — without this field the number would be silently
    /// dropped by serde. Convention: higher = more likely "yes".
    #[serde(default)]
    pub noul: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct JevResponse {
    #[serde(default)]
    answers: std::collections::BTreeMap<String, JevAnswer>,
}

/// The abstract decision service the engine codes against. Production uses
/// [`JevClient`]; tests use stubs.
pub trait JevDecision: Send + Sync {
    /// Evaluate `state` against the given questions. Returns the answer map
    /// keyed by question name. `Err` means "unavailable — use heuristics".
    fn ask(
        &self,
        state: &str,
        questions: &[(String, JevQuestion)],
    ) -> impl std::future::Future<
        Output = Result<std::collections::BTreeMap<String, JevAnswer>, JevError>,
    > + Send;
}

/// HTTP client for the Jev System One API.
#[derive(Clone)]
pub struct JevClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl JevClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_options(JEV_DEFAULT_ENDPOINT, api_key, "jev-latest")
    }

    pub fn with_options(endpoint: &str, api_key: impl Into<String>, model: &str) -> Self {
        Self {
            // reqwest snapshots proxy env at build time; apply_proxy_env runs
            // before any client construction in the CLI.
            http: reqwest::Client::builder()
                .timeout(JEV_TIMEOUT)
                .build()
                .expect("reqwest client with plain timeout"),
            endpoint: endpoint.trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.to_string(),
        }
    }

    pub async fn ask_raw(
        &self,
        state: &str,
        questions: &[(String, JevQuestion)],
    ) -> Result<std::collections::BTreeMap<String, JevAnswer>, JevError> {
        let mut questions_json = serde_json::Map::new();
        for (name, q) in questions {
            let value = serde_json::to_value(q)
                .map_err(|e| JevError::Parse(format!("encode question {name}: {e}")))?;
            questions_json.insert(name.clone(), value);
        }
        let body = serde_json::json!({
            "state": state,
            "model": self.model,
            "questions": questions_json,
        });
        // Jev is a paid external API on a cold path: every round trip is logged
        // so call count / latency / failure rate stay observable even though
        // callers fail-open silently.
        let started = std::time::Instant::now();
        let resp = match self
            .http
            .post(format!("{}/v1/systemone", self.endpoint))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!(
                    model = %self.model,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %e,
                    "jev transport error"
                );
                return Err(JevError::Transport(e));
            }
        };
        let status = resp.status();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(model = %self.model, status = %status, elapsed_ms, "jev non-success status");
            return Err(JevError::Parse(format!("HTTP {status}: {text}")));
        }
        let parsed: JevResponse = resp.json().await.map_err(|e| {
            tracing::warn!(model = %self.model, status = %status, elapsed_ms, error = %e, "jev response decode failed");
            JevError::Parse(format!("decode: {e}"))
        })?;
        tracing::debug!(
            model = %self.model,
            status = %status,
            elapsed_ms,
            questions = questions.len(),
            answers = parsed.answers.len(),
            "jev call ok"
        );
        Ok(parsed.answers)
    }
}

impl JevDecision for JevClient {
    async fn ask(
        &self,
        state: &str,
        questions: &[(String, JevQuestion)],
    ) -> Result<std::collections::BTreeMap<String, JevAnswer>, JevError> {
        self.ask_raw(state, questions).await
    }
}

impl JevClient {
    /// Ask a single yes/no (noul) question and return P(yes), applying the
    /// global confidence floor. `None` means "no trusted answer — keep the
    /// heuristic path".
    pub async fn ask_noul(&self, state: &str, instructions: &str) -> Option<f64> {
        let answers = self
            .ask_raw(state, &[("q".to_string(), JevQuestion::noul(instructions))])
            .await
            .ok()?;
        let a = answers.get("q")?;
        if let Some(conf) = a.confidence {
            if conf < JEV_MIN_CONFIDENCE {
                return None;
            }
        }
        if let Some(p) = a.noul {
            return Some(p);
        }
        // Fallback for gateways that return a boolean `answer` instead of a
        // calibrated `noul` probability.
        match a.answer.as_ref().and_then(|v| v.as_bool()) {
            Some(true) => Some(1.0),
            Some(false) => Some(0.0),
            None => None,
        }
    }
}

/// Process-wide Jev client, initialized once at startup when configured.
/// Lives in the api crate (not engine) so retry/transient classification in
/// this crate can use it without an upward dependency.
static GLOBAL_CLIENT: OnceLock<Option<JevClient>> = OnceLock::new();

/// Install the process-wide client. Called from engine bootstrap; later
/// calls are ignored (first writer wins, matching the old engine-side
/// OnceLock semantics).
pub fn set_global_client(client: JevClient) {
    let _ = GLOBAL_CLIENT.set(Some(client));
}

/// Snapshot of the process-wide client, if configured.
pub fn global_client() -> Option<JevClient> {
    GLOBAL_CLIENT.get().and_then(|c| c.clone())
}

/// Convenience: one noul question against the process-wide client.
/// `None` when unconfigured or the call fails — callers must treat that as
/// "keep current behavior".
pub async fn global_noul(state: &str, instructions: &str) -> Option<f64> {
    let client = global_client()?;
    client.ask_noul(state, instructions).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_serialization_roundtrip_shapes() {
        let q = JevQuestion::choice(
            "Classify the run outcome.",
            &[
                ("clean_success", "Verified completion"),
                ("truncation", "Cut off mid-output"),
            ],
        );
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["type"], "choice");
        assert!(v["criteria"]["clean_success"].is_string());

        let n = JevQuestion::noul("Does this detail mention urgency?");
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["type"], "noul");
        assert!(v.get("criteria").is_none());
    }

    #[test]
    fn response_parse_tolerates_missing_fields() {
        let raw = r#"{
            "answers": {
                "category": {
                    "choice": "billing",
                    "probabilities": {"billing": 0.88, "technical": 0.12},
                    "confidence": 0.81
                }
            }
        }"#;
        let parsed: JevResponse = serde_json::from_str(raw).unwrap();
        let cat = parsed.answers.get("category").unwrap();
        assert_eq!(cat.choice.as_deref(), Some("billing"));
        assert_eq!(cat.probabilities["billing"], 0.88);
    }

    #[test]
    fn response_parse_captures_noul_probability() {
        // Live wire shape (verified 2026-09-21): the noul probability is a
        // top-level "noul" float, not a boolean `answer`.
        let raw = r#"{
            "answers": {
                "q": {"type": "noul", "noul": 0.1}
            }
        }"#;
        let parsed: JevResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.answers["q"].noul, Some(0.1));
        assert!(parsed.answers["q"].answer.is_none());
    }

    #[tokio::test]
    async fn global_client_unset_means_none() {
        // Nothing in this test binary installs a client; the engine-side
        // setter runs only in the serve process.
        assert!(global_client().is_none());
        assert!(global_noul("state", "is anything wrong?").await.is_none());
    }
}
