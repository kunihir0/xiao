use std::time::Duration;

use rand::RngExt;

pub const MAX_RETRY_ATTEMPTS: u32 = 32;
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(15 * 60);
pub const MIN_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    initial: Duration,
    maximum: Duration,
    jitter_percent: u8,
    max_attempts: u32,
}
impl RetryPolicy {
    /// Creates a policy with a non-zero minimum initial backoff.
    pub fn new(initial: Duration, maximum: Duration, jitter_percent: u8) -> Self {
        let maximum = maximum.clamp(MIN_RETRY_DELAY, MAX_RETRY_DELAY);
        let initial = initial.clamp(MIN_RETRY_DELAY, maximum);
        Self {
            initial,
            maximum,
            jitter_percent: if jitter_percent > 100 {
                100
            } else {
                jitter_percent
            },
            max_attempts: 5,
        }
    }
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.min(MAX_RETRY_ATTEMPTS);
        self
    }
    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        self.max_attempts
    }
    #[must_use]
    pub fn delay(&self, attempt: u32, jitter: u32) -> Duration {
        let shift = attempt.min(31);
        let base = self
            .initial
            .as_millis()
            .saturating_mul(1u128 << shift)
            .min(self.maximum.as_millis());
        let spread = base.saturating_mul(self.jitter_percent as u128) / 100;
        let offset = if spread == 0 {
            0
        } else {
            u128::from(jitter) % (spread.saturating_mul(2).saturating_add(1))
        };
        let value = base
            .saturating_sub(spread)
            .saturating_add(offset)
            .min(self.maximum.as_millis());
        Duration::from_millis(value.min(u128::from(u64::MAX)) as u64)
    }
    /// Calculates a delay with fresh OS-seeded randomness for production
    /// reconnects. Tests can use [`Self::delay`] with a deterministic value.
    #[must_use]
    pub fn random_delay(&self, attempt: u32) -> Duration {
        let base = self.delay(attempt, 0);
        let spread = base.as_millis().saturating_mul(self.jitter_percent as u128) / 100;
        if spread == 0 {
            return base;
        }
        let mut rng = rand::rng();
        let jitter = rng.random_range(0..=spread.saturating_mul(2));
        let millis = base
            .as_millis()
            .saturating_sub(spread)
            .saturating_add(jitter)
            .min(self.maximum.as_millis())
            .min(u128::from(u64::MAX));
        Duration::from_millis(millis as u64)
    }
    #[must_use]
    pub const fn maximum(&self) -> Duration {
        self.maximum
    }
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(30), 20)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_and_deterministic() {
        let policy = RetryPolicy::new(Duration::from_secs(1), Duration::from_secs(8), 25);
        assert_eq!(policy.delay(2, 7), policy.delay(2, 7));
        assert!(policy.delay(20, 99) <= Duration::from_secs(8));
    }

    #[test]
    fn minimum_initial_backoff_is_non_zero() {
        let policy = RetryPolicy::new(Duration::ZERO, Duration::ZERO, 0);
        assert_eq!(policy.delay(0, 0), MIN_RETRY_DELAY);
    }
    #[test]
    fn attempts_are_hard_bounded() {
        assert_eq!(
            RetryPolicy::default()
                .with_max_attempts(u32::MAX)
                .max_attempts(),
            MAX_RETRY_ATTEMPTS
        );
    }
}
