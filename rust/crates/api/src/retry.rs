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
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !error.is_retryable() || attempt >= cfg.max_attempts {
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
}
