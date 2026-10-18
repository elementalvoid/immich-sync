//! Exponential backoff with full jitter (SPEC R6.1, R6.2).
//!
//! The wait before a later attempt is a uniform draw from `0` up to an
//! exponential ceiling, which is what the spec's "full jitter" means: the
//! ceiling grows with every failure, so a stopped server sees a handful of
//! requests, not a hot loop, and two clients that fail together do not retry
//! in lockstep.
//!
//! The module is the whole policy: the engine decides *when* an item may be
//! tried again, and this decides *how long* the client waits. A server-supplied
//! `Retry-After` is a floor on that wait (R6.2), itself capped, so a broken or
//! hostile server cannot park the queue for a week.

use std::time::Duration;

/// How long the client waits before a later attempt (R6.1, R6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The first ceiling. The wait after the first failure is somewhere in
    /// `0..=base` (R6.1).
    pub base: Duration,
    /// The longest the exponential ceiling may grow to (R6.1: "capped").
    pub max: Duration,
    /// The longest a `Retry-After` may hold one item (R6.2). A server that
    /// asks for longer than this is answered with this wait, not with its own.
    pub max_retry_after: Duration,
    /// A fixed jitter seed, so a test can pin the waits it observes. Production
    /// leaves this `None` and every draw comes from the operating system.
    pub seed: Option<u64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(10 * 60),
            seed: None,
        }
    }
}

impl RetryPolicy {
    /// The wait before the next attempt, after `attempt` failures.
    ///
    /// `attempt` is 1 for the first failure, so the ceiling doubles with every
    /// later one. `retry_after` is the server's own `Retry-After` hint, when it
    /// sent one (R6.2).
    #[must_use]
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let mut rng = match self.seed {
            // The attempt joins the seed, so a pinned policy still draws a
            // different roll for every attempt instead of one constant delay.
            Some(seed) => fastrand::Rng::with_seed(seed.wrapping_add(u64::from(attempt))),
            None => fastrand::Rng::new(),
        };
        self.delay_with(attempt, retry_after, rng.f64())
    }

    /// The wait for one jitter roll in `[0, 1)`, so a test can pin the value
    /// `delay` would draw.
    #[must_use]
    pub fn delay_with(&self, attempt: u32, retry_after: Option<Duration>, roll: f64) -> Duration {
        let jittered = self.ceiling(attempt).mul_f64(roll.clamp(0.0, 1.0));
        match retry_after {
            Some(wait) => jittered.max(wait.min(self.max_retry_after)),
            None => jittered,
        }
    }

    /// The full-jitter ceiling after `attempt` failures: `base * 2^(attempt-1)`,
    /// never past `max`.
    #[must_use]
    pub fn ceiling(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(31);
        self.base.saturating_mul(1_u32 << exponent).min(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_jitter_is_full_and_the_ceiling_grows() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay_with(1, None, 0.0), Duration::ZERO);
        assert_eq!(policy.delay_with(1, None, 0.5), Duration::from_millis(500));
        assert_eq!(policy.delay_with(1, None, 1.0), Duration::from_secs(1));
        assert_eq!(policy.delay_with(3, None, 1.0), Duration::from_secs(4));
        assert!(
            policy.ceiling(1) < policy.ceiling(2)
                && policy.ceiling(2) < policy.ceiling(3)
                && policy.ceiling(3) < policy.ceiling(4),
            "R6.1: the ceiling grows with every failure"
        );
        assert_eq!(
            policy.delay_with(20, None, 1.0),
            Duration::from_secs(30),
            "R6.1: the ceiling is capped"
        );
    }

    #[test]
    fn a_retry_after_is_a_floor_and_is_itself_capped() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.delay_with(1, Some(Duration::from_secs(7)), 0.0),
            Duration::from_secs(7),
            "R6.2: the server's wait wins over a small jitter"
        );
        assert_eq!(
            policy.delay_with(1, Some(Duration::from_secs(7)), 1.0),
            Duration::from_secs(7),
            "R6.2: and it wins over a small ceiling too"
        );
        assert_eq!(
            policy.delay_with(4, Some(Duration::from_secs(7)), 1.0),
            Duration::from_secs(8),
            "the larger of the two waits is the one taken"
        );
        assert_eq!(
            policy.delay_with(1, Some(Duration::from_secs(86_400)), 0.0),
            Duration::from_secs(600),
            "a hostile Retry-After cannot park the queue for a day"
        );
    }

    #[test]
    fn a_pinned_policy_is_reproducible_and_within_the_ceiling() {
        let pinned = RetryPolicy {
            seed: Some(7),
            ..RetryPolicy::default()
        };
        for attempt in 1..=8 {
            let delay = pinned.delay(attempt, None);
            assert!(
                delay <= pinned.ceiling(attempt),
                "attempt {attempt}: {delay:?} is past its ceiling {:?}",
                pinned.ceiling(attempt)
            );
            assert_eq!(
                delay,
                pinned.delay(attempt, None),
                "attempt {attempt}: a pinned policy must not drift"
            );
        }
        assert_ne!(
            pinned.delay(1, None),
            pinned.delay(2, None),
            "the jitter is not one constant fraction"
        );
    }
}
