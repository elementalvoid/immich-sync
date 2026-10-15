//! The circuit breaker (SPEC R6.6).
//!
//! A run of connection failures means the server is gone, not that one request
//! was unlucky. Retrying each item against a dead server is what makes a client
//! look like an attack, so after a run of connection failures the breaker opens
//! and the queue waits. Before it resumes, one credential-validation probe asks
//! whether the server is back (R6.6).
//!
//! The breaker is plain shared state: the engine asks whether it is open, feeds
//! it connection failures, and reports the probe's outcome.
//!
//! The connection counts the same run: `EngineConfig::breaker_threshold` is
//! also the number of consecutive failed requests after which the connection
//! needs the user, so the breaker opens at the point the tray changes and a
//! success resets both notions.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The run of connection failures that trips the breaker (R6.6).
#[derive(Debug)]
pub struct CircuitBreaker {
    threshold: u32,
    cooldown: Duration,
    state: Mutex<BreakerState>,
}

/// The breaker's live state.
#[derive(Debug, Default)]
struct BreakerState {
    /// The current run of connection failures.
    failures: u32,
    /// When the breaker last opened or last failed its probe.
    opened_at: Option<Instant>,
}

impl CircuitBreaker {
    /// Builds a breaker that trips after `threshold` connection failures and
    /// waits `cooldown` between probes.
    #[must_use]
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown,
            state: Mutex::new(BreakerState::default()),
        }
    }

    /// Whether the breaker is open: new requests must wait for the probe.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.state.lock().expect("the breaker lock").failures >= self.threshold
    }

    /// How long until the probe may run, when the breaker is open.
    ///
    /// `None` means closed: the request may go out.
    #[must_use]
    pub fn open_for(&self) -> Option<Duration> {
        let state = self.state.lock().expect("the breaker lock");
        if state.failures < self.threshold {
            return None;
        }
        let opened = state.opened_at.unwrap_or_else(Instant::now);
        Some(self.cooldown.saturating_sub(opened.elapsed()))
    }

    /// Records one connection failure. Returns whether the breaker is open
    /// after it.
    ///
    /// Every failure while open restarts the cooldown, so a probe that fails
    /// does not turn into a probe loop.
    pub fn record_failure(&self) -> bool {
        let mut state = self.state.lock().expect("the breaker lock");
        state.failures = state.failures.saturating_add(1);
        if state.failures >= self.threshold {
            state.opened_at = Some(Instant::now());
            return true;
        }
        false
    }

    /// Records a success: the run resets and the breaker closes.
    pub fn record_success(&self) {
        let mut state = self.state.lock().expect("the breaker lock");
        state.failures = 0;
        state.opened_at = None;
    }

    /// The current run of connection failures, for a status line or a test.
    #[must_use]
    pub fn failures(&self) -> u32 {
        self.state.lock().expect("the breaker lock").failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_of_failures_opens_the_breaker_and_a_success_closes_it() {
        let breaker = CircuitBreaker::new(3, Duration::from_millis(50));
        assert!(!breaker.is_open());
        assert_eq!(breaker.open_for(), None);

        assert!(!breaker.record_failure());
        assert!(!breaker.record_failure());
        assert!(!breaker.is_open(), "two failures are below the threshold");
        assert!(breaker.record_failure(), "the third failure trips it");
        assert!(breaker.is_open());
        assert!(
            breaker.open_for().is_some(),
            "R6.6: an open breaker makes the caller wait for the probe"
        );

        breaker.record_success();
        assert!(!breaker.is_open());
        assert_eq!(breaker.open_for(), None);
        assert_eq!(breaker.failures(), 0);
    }

    #[test]
    fn a_failed_probe_restarts_the_cooldown_instead_of_probing_again() {
        let breaker = CircuitBreaker::new(1, Duration::from_millis(100));
        assert!(breaker.record_failure());
        std::thread::sleep(Duration::from_millis(20));
        let first = breaker.open_for().expect("the breaker is open").as_millis();
        assert!(first < 100, "the cooldown ran down: {first}ms");

        // The probe failed: the cooldown starts over, so the next probe is a
        // full cooldown away rather than immediate.
        assert!(breaker.record_failure());
        let second = breaker.open_for().expect("still open").as_millis();
        assert!(
            second > first,
            "a failed probe must not shorten the wait: {first}ms then {second}ms"
        );
    }
}
