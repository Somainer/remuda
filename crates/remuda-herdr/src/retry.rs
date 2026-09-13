//! Bounded wait for a shutting-down Herdr server to release its socket.
//!
//! A Herdr session server is named after the Node's data dir, so a restarted
//! Node targets the socket the *previous* server is still unlinking. Herdr
//! keeps answering `ping` through that window while refusing every other
//! method with `server_unavailable: server is shutting down`, so "the socket
//! answers" is not proof the server is usable. This module holds the pure
//! timing policy; [`crate::HerdrServer`] applies it.

use std::time::Duration;

/// Default ceiling on waiting for a predecessor to exit.
///
/// Observed shutdown is well under a second for an idle server and grows with
/// pane count and processes that ignore `SIGTERM`; two minutes covers the
/// worst local-demo case with room to spare.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(120);

/// First backoff step.
const INITIAL_BACKOFF: Duration = Duration::from_millis(50);

/// Backoff ceiling; polling stays responsive so a fast exit is noticed fast.
const MAX_BACKOFF: Duration = Duration::from_millis(1_000);

/// Exponential backoff bounded by both a per-step ceiling and a total budget.
///
/// Deliberately clock-free: callers feed it elapsed time so tests can drive it
/// deterministically.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    max_wait: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_wait: DEFAULT_MAX_WAIT,
            initial_backoff: INITIAL_BACKOFF,
            max_backoff: MAX_BACKOFF,
        }
    }
}

impl RetryPolicy {
    /// Policy with an explicit total budget and the default backoff shape.
    #[must_use]
    pub fn with_max_wait(max_wait: Duration) -> Self {
        Self {
            max_wait,
            ..Self::default()
        }
    }

    /// Override the backoff step bounds (tests use short steps).
    #[must_use]
    pub fn with_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_backoff = initial;
        self.max_backoff = max.max(initial);
        self
    }

    /// Total budget before the caller must give up waiting.
    #[must_use]
    pub fn max_wait(&self) -> Duration {
        self.max_wait
    }

    /// Sleep before attempt `attempt` (0-based), or `None` once `elapsed` has
    /// consumed the budget.
    ///
    /// The returned delay never overshoots the remaining budget, so a caller
    /// that always sleeps the full amount still finishes within `max_wait`.
    #[must_use]
    pub fn backoff(&self, attempt: u32, elapsed: Duration) -> Option<Duration> {
        let remaining = self.max_wait.checked_sub(elapsed)?;
        if remaining.is_zero() {
            return None;
        }
        let step = self
            .initial_backoff
            .saturating_mul(1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX))
            .min(self.max_backoff);
        Some(step.min(remaining))
    }
}

/// Why a bounded wait stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The endpoint became usable (a non-`ping` call succeeded).
    Ready,
    /// It was still unusable when the budget ran out.
    TimedOut,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_saturates() {
        let policy = RetryPolicy::default();
        let zero = Duration::ZERO;
        assert_eq!(policy.backoff(0, zero), Some(Duration::from_millis(50)));
        assert_eq!(policy.backoff(1, zero), Some(Duration::from_millis(100)));
        assert_eq!(policy.backoff(2, zero), Some(Duration::from_millis(200)));
        assert_eq!(policy.backoff(5, zero), Some(MAX_BACKOFF));
        // A very large attempt count must saturate, never overflow or wrap.
        assert_eq!(policy.backoff(u32::MAX, zero), Some(MAX_BACKOFF));
    }

    #[test]
    fn budget_exhaustion_ends_the_wait() {
        let policy = RetryPolicy::with_max_wait(Duration::from_secs(1));
        assert!(policy.backoff(0, Duration::from_millis(999)).is_some());
        assert_eq!(policy.backoff(0, Duration::from_secs(1)), None);
        assert_eq!(policy.backoff(0, Duration::from_secs(2)), None);
    }

    #[test]
    fn final_sleep_never_overshoots_the_budget() {
        let policy = RetryPolicy::with_max_wait(Duration::from_secs(1));
        // 30ms left, but the backoff step would be 1s: the step is clamped so
        // the caller wakes exactly at the deadline rather than past it.
        let remaining = policy
            .backoff(9, Duration::from_millis(970))
            .expect("budget remains");
        assert_eq!(remaining, Duration::from_millis(30));
    }

    #[test]
    fn custom_backoff_keeps_initial_as_the_floor() {
        let policy = RetryPolicy::with_max_wait(Duration::from_secs(5))
            .with_backoff(Duration::from_millis(10), Duration::from_millis(1));
        // A max below the initial step must not invert the bounds.
        assert_eq!(
            policy.backoff(0, Duration::ZERO),
            Some(Duration::from_millis(10))
        );
        assert_eq!(
            policy.backoff(8, Duration::ZERO),
            Some(Duration::from_millis(10))
        );
    }
}
