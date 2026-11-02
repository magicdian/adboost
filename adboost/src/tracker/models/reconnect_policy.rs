use std::time::Duration;

/// Bounded exponential-backoff policy for tracker reconnects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Delay before the first reconnect attempt.
    pub initial_backoff: Duration,
    /// Upper bound for the exponential delay.
    pub max_backoff: Duration,
    /// Maximum number of consecutive reconnect attempts; `None` retries
    /// forever, `Some(0)` disables retries.
    pub max_attempts: Option<u32>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
            max_attempts: None,
        }
    }
}

impl ReconnectPolicy {
    pub(crate) fn allows_attempt(&self, attempt: u32) -> bool {
        self.max_attempts.is_none_or(|max| attempt < max)
    }

    pub(crate) fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
        let scaled = self
            .initial_backoff
            .as_millis()
            .saturating_mul(u128::from(factor));
        let capped = scaled.min(self.max_backoff.as_millis());
        Duration::from_millis(u64::try_from(capped).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::ReconnectPolicy;

    #[test]
    fn default_backoff_is_exponential_and_capped() {
        let policy = ReconnectPolicy::default();
        assert_eq!(policy.backoff(0), Duration::from_millis(250));
        assert_eq!(policy.backoff(1), Duration::from_millis(500));
        assert_eq!(policy.backoff(2), Duration::from_millis(1_000));
        assert_eq!(policy.backoff(10), Duration::from_secs(5));
        assert_eq!(policy.backoff(u32::MAX), Duration::from_secs(5));
    }

    #[test]
    fn attempt_limit_counts_from_zero() {
        let policy = ReconnectPolicy {
            max_attempts: Some(2),
            ..ReconnectPolicy::default()
        };
        assert!(policy.allows_attempt(0));
        assert!(policy.allows_attempt(1));
        assert!(!policy.allows_attempt(2));

        let no_retries = ReconnectPolicy {
            max_attempts: Some(0),
            ..ReconnectPolicy::default()
        };
        assert!(!no_retries.allows_attempt(0));
    }
}
