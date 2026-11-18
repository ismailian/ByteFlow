//! Retry policy (spec §7.2): up to 5 attempts, exponential backoff with full
//! jitter — uniform in `[0, min(500 ms × 2^(n-1), 30 s)]` — and only for
//! `NetworkFailure`.

use std::time::Duration;

use rand::Rng;

use bf_core::AppFailure;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 5, base: Duration::from_millis(500), cap: Duration::from_secs(30) }
    }
}

impl RetryPolicy {
    /// Upper bound of the jitter window before attempt `n + 1` (n ≥ 1).
    pub fn ceiling(&self, attempt: u32) -> Duration {
        let exp = self.base.saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
        exp.min(self.cap)
    }

    pub fn delay(&self, attempt: u32) -> Duration {
        let ceil = self.ceiling(attempt).as_millis() as u64;
        Duration::from_millis(rand::rng().random_range(0..=ceil))
    }

    /// Whether to retry after `attempt` failed attempts with `err`.
    pub fn should_retry(&self, attempt: u32, err: &AppFailure) -> bool {
        attempt < self.max_attempts && err.is_retryable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy() {
        let p = RetryPolicy::default();
        assert_eq!(p.ceiling(1), Duration::from_millis(500));
        assert_eq!(p.ceiling(2), Duration::from_millis(1000));
        assert_eq!(p.ceiling(10), Duration::from_secs(30));
        for _ in 0..100 {
            assert!(p.delay(3) <= Duration::from_millis(2000));
        }
        assert!(p.should_retry(1, &AppFailure::network("x")));
        assert!(!p.should_retry(5, &AppFailure::network("x")));
        assert!(!p.should_retry(1, &AppFailure::permission("x")));
    }
}
