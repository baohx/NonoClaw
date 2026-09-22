//! Bounded retry strategy for transient failures before a response stream starts.

use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nonoclaw_core::Error;

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Maximum wall-clock time spent retrying, including sleeps.
    pub max_elapsed: Duration,
    /// Symmetric jitter around the exponential delay, as a percentage.
    pub jitter_percent: u8,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            max_attempts: 6,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            max_elapsed: Duration::from_secs(90),
            jitter_percent: 20,
        }
    }
}

impl RetryConfig {
    /// Deterministic exponential backoff before jitter.
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let exp = attempt.saturating_sub(1);
        let ms = self
            .initial_backoff
            .as_millis()
            .saturating_mul(1u128 << exp.min(20));
        Duration::from_millis(ms.min(self.max_backoff.as_millis()) as u64)
    }

    /// Jittered backoff. `entropy` is explicit so tests can verify bounds.
    pub fn jittered_backoff_for(&self, attempt: u32, entropy: u64) -> Duration {
        let base_ms = self.backoff_for(attempt).as_millis();
        let jitter = u128::from(self.jitter_percent.min(100));
        if jitter == 0 || base_ms == 0 {
            return Duration::from_millis(base_ms as u64);
        }
        let span = jitter.saturating_mul(2).saturating_add(1);
        let percent = 100u128
            .saturating_sub(jitter)
            .saturating_add(u128::from(entropy) % span);
        let jittered = base_ms.saturating_mul(percent) / 100;
        Duration::from_millis(jittered.min(self.max_backoff.as_millis()) as u64)
    }
}

fn retry_entropy(attempt: u32) -> u64 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    time ^ u64::from(attempt).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Substrings that hint a "permanent" classification might actually be a
/// transient infrastructure fault (gateways that wrap network failures in
/// 4xx/5xx bodies, SSE frames with `finish_reason: network_error`, etc.).
const TRANSIENT_HINTS: &[&str] = &[
    "network",
    "timeout",
    "timed out",
    "connection",
    "reset",
    "broken pipe",
    "eof",
    "upstream",
    "gateway",
    "temporarily",
    "unavailable",
    "overload",
    "stream",
];

/// Cheap text pre-screen: does this error text even smell transient?
pub(crate) fn looks_transient(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    TRANSIENT_HINTS.iter().any(|h| lower.contains(h))
}

/// Errors that may be reconsidered for retry via a Jev second opinion.
/// Structural failures (auth, cancelled, prompt-too-long, tool, permission,
/// config, io) are never rescued — retrying them is meaningless or harmful.
/// Returns the state text to evaluate, or `None` to keep the verdict.
fn rescue_candidate(error: &Error) -> Option<String> {
    let text = match error {
        Error::Api {
            status,
            message,
            kind: nonoclaw_core::ApiErrorKind::NonRetryable,
        } => format!("HTTP {status}: {message}"),
        Error::Other(msg) => msg.clone(),
        _ => return None,
    };
    if looks_transient(&text) {
        Some(text)
    } else {
        None
    }
}

/// Calibrated P(yes) needed for Jev to override a non-retryable verdict.
const JEV_RESCUE_THRESHOLD: f64 = 0.6;

/// One Jev noul question: "is this actually transient?". `false` means keep
/// the heuristic verdict (fail-open to current behavior).
async fn jev_rescues_as_transient(error: &Error) -> bool {
    let Some(text) = rescue_candidate(error) else {
        return false;
    };
    let state = format!(
        "An LLM provider request failed before any content was produced. \
         Underlying error: {text}"
    );
    let instructions = "Is this a transient infrastructure fault (network \
        dropout, gateway/upstream hiccup, momentary overload) that would \
        plausibly succeed on an immediate retry, rather than a permanent \
        error of the request itself (bad payload, auth, policy)?";
    crate::jev::global_noul(&state, instructions)
        .await
        .is_some_and(|p| p >= JEV_RESCUE_THRESHOLD)
}

/// Run `op` with bounded, jittered retries and notify before each retry sleep.
pub async fn with_retry_notify<T, F, Fut, N>(
    cfg: &RetryConfig,
    mut op: F,
    mut notify: N,
) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
    N: FnMut(u32, Duration, &Error),
{
    let started = Instant::now();
    let mut attempt = 1u32;
    // At most one Jev second opinion per with_retry call, regardless of how
    // many non-retryable errors the op produces.
    let mut jev_asked = false;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                let give_up = attempt >= cfg.max_attempts || {
                    if error.is_retryable() {
                        false
                    } else if jev_asked {
                        true
                    } else {
                        jev_asked = true;
                        !jev_rescues_as_transient(&error).await
                    }
                };
                if give_up {
                    return Err(error);
                }
                let delay = cfg.jittered_backoff_for(attempt, retry_entropy(attempt));
                if started.elapsed().saturating_add(delay) > cfg.max_elapsed {
                    return Err(error);
                }
                notify(attempt + 1, delay, &error);
                tracing::warn!(
                    next_attempt = attempt + 1,
                    max_attempts = cfg.max_attempts,
                    ?delay,
                    "retryable API error, backing off"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

/// Compatibility wrapper for callers that do not need retry trace events.
pub async fn with_retry<T, F, Fut>(cfg: &RetryConfig, op: F) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    with_retry_notify(cfg, op, |_, _, _| {}).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_caps_and_jitter_stays_bounded() {
        let cfg = RetryConfig {
            max_attempts: 6,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(1000),
            max_elapsed: Duration::from_secs(10),
            jitter_percent: 20,
        };
        assert_eq!(cfg.backoff_for(1), Duration::from_millis(100));
        assert_eq!(cfg.backoff_for(2), Duration::from_millis(200));
        assert_eq!(cfg.backoff_for(5), Duration::from_millis(1000));
        for entropy in 0..100 {
            let delay = cfg.jittered_backoff_for(3, entropy);
            assert!(delay >= Duration::from_millis(320));
            assert!(delay <= Duration::from_millis(480));
        }
    }

    #[tokio::test]
    async fn total_elapsed_bound_prevents_another_attempt() {
        let cfg = RetryConfig {
            max_attempts: 10,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(1),
            max_elapsed: Duration::ZERO,
            jitter_percent: 0,
        };
        let mut calls = 0;
        let result: Result<(), Error> = with_retry(&cfg, || {
            calls += 1;
            async { Err(Error::Network("offline".into())) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn rescue_candidate_matrix() {
        use nonoclaw_core::ApiErrorKind;
        // Non-retryable API error with transient hint → candidate.
        let e = Error::Api {
            status: 400,
            message: "upstream network_error".into(),
            kind: ApiErrorKind::NonRetryable,
        };
        assert!(rescue_candidate(&e).is_some());
        // Non-retryable without hint → not a candidate (no Jev call).
        let e = Error::Api {
            status: 400,
            message: "invalid request: unknown field".into(),
            kind: ApiErrorKind::NonRetryable,
        };
        assert!(rescue_candidate(&e).is_none());
        // Retryable API errors never reach the rescue path (already retried).
        let e = Error::Api {
            status: 503,
            message: "unavailable".into(),
            kind: ApiErrorKind::Retryable,
        };
        assert!(rescue_candidate(&e).is_none());
        // Structural errors are never rescued.
        assert!(rescue_candidate(&Error::Cancelled).is_none());
        assert!(rescue_candidate(&Error::Timeout).is_none());
        assert!(
            rescue_candidate(&Error::Auth("bad key (connection reset?)".into())).is_none()
        );
        assert!(rescue_candidate(&Error::PromptTooLong("too long".into())).is_none());
        assert!(
            rescue_candidate(&Error::Tool {
                tool: "Edit".into(),
                message: "stream broke".into()
            })
            .is_none()
        );
        assert!(rescue_candidate(&Error::Config("bad upstream url".into())).is_none());
        // Other() with transient hint is the SSE/gateway escape hatch.
        assert!(rescue_candidate(&Error::Other("stream ended: eof".into())).is_some());
        // Other() without hint stays dead.
        assert!(rescue_candidate(&Error::Other("malformed payload".into())).is_none());
    }

    #[test]
    fn looks_transient_matches_gateway_phrases() {
        assert!(looks_transient("finish_reason: network_error"));
        assert!(looks_transient("Connection reset by peer"));
        assert!(looks_transient("502 Bad Gateway"));
        assert!(!looks_transient("unknown field `ts`"));
        assert!(!looks_transient("401 unauthorized"));
    }

    #[tokio::test]
    async fn non_retryable_without_jev_still_fails_fast() {
        // No global client installed in tests → Jev path is a no-op and the
        // pre-existing fail-fast behavior must be preserved exactly.
        let cfg = RetryConfig {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            max_elapsed: Duration::from_secs(5),
            jitter_percent: 0,
        };
        let mut calls = 0;
        let result: Result<(), Error> = with_retry(&cfg, || {
            calls += 1;
            async {
                Err(Error::Api {
                    status: 400,
                    message: "network glitch".into(),
                    kind: nonoclaw_core::ApiErrorKind::NonRetryable,
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
}
