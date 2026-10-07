use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    initial: Duration,
    maximum: Duration,
    jitter_percent: u8,
    max_attempts: u32,
}
impl RetryPolicy {
    #[must_use]
    pub const fn new(initial: Duration, maximum: Duration, jitter_percent: u8) -> Self {
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
    #[must_use]
    pub const fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
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
        Duration::from_millis(value as u64)
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
}
