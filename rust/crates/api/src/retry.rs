//! Retry strategy for transient API failures. Mirrors the exponential-backoff
//! behavior in `src/services/api/withRetry.ts`. For the Phase 0 slice we retry
//! only failures that occur before the stream starts (connection, 429/5xx on
//! the initial response); mid-stream errors surface as terminal.

use std::future::Future;
use std::time::Duration;

use nonoclaw_core::Error;

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            max_attempts: 6,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
        }
    }
}

impl RetryConfig {
    /// Backoff for the given (1-based) attempt number: `initial * 2^(n-1)`,
    /// capped at `max_backoff`.
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let exp = attempt.saturating_sub(1);
        let ms = self
            .initial_backoff
            .as_millis()
            .saturating_mul(1u128 << exp.min(20));
        let capped = ms.min(self.max_backoff.as_millis());
        Duration::from_millis(capped as u64)
    }
}

/// Run `op` up to `cfg.max_attempts` times, retrying when it returns a
/// retryable [`Error`]. Sleeps the configured backoff between attempts.
pub async fn with_retry<T, F, Fut>(cfg: &RetryConfig, mut op: F) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    let mut attempt = 1u32;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !e.is_retryable() || attempt >= cfg.max_attempts {
                    return Err(e);
                }
                let backoff = cfg.backoff_for(attempt);
                tracing::warn!(
                    attempt,
                    max_attempts = cfg.max_attempts,
                    ?backoff,
                    "retryable API error, backing off"
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let cfg = RetryConfig {
            max_attempts: 6,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(1000),
        };
        assert_eq!(cfg.backoff_for(1), Duration::from_millis(100));
        assert_eq!(cfg.backoff_for(2), Duration::from_millis(200));
        assert_eq!(cfg.backoff_for(3), Duration::from_millis(400));
        assert_eq!(cfg.backoff_for(4), Duration::from_millis(800));
        assert_eq!(cfg.backoff_for(5), Duration::from_millis(1000)); // capped
        assert_eq!(cfg.backoff_for(50), Duration::from_millis(1000));
    }
}
