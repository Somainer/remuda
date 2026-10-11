//! One synchronized producer state shared by the async transcript pump, the
//! blocking collect/finalise closures, the abort-residual thread and the
//! streaming flusher (c-ctxusage r9 item 2).
//!
//! Before this gate the pieces coordinated through two loose atomics
//! (`collect_in_flight` + an empty hydrator slot, with a separate
//! `residual_done` the abort side set on a timer): the abort guard could read
//! "no producer, empty slot" in the gap between a QUEUED blocking closure and
//! its claim, declare completion, and let the flusher exit before that
//! closure parked its frames. Both guards also treated an elapsed ~120 s
//! wait budget as completion.
//!
//! The state machine makes that gap unobservable:
//!
//! ```text
//! Idle(count=0) ──register()──▶ Active(n≥1) ──last completion──▶ Idle
//!                                   │                              │
//!                                   └──────── retire() ───────────┴──▶ Retired
//! ```
//!
//! * A producer is registered SYNCHRONOUSLY by the pump task, before
//!   `spawn_blocking` enqueues the closure, so a closure merely queued on the
//!   blocking pool is already `Active`: the abort decision can never run
//!   ahead of it.
//! * The hydrator lives behind the SAME lock as the producer count, so
//!   claiming the hydrator, owning the slot and deciding completion are one
//!   atomic state.
//! * [`ProducerGate::retire`] is the ONLY completion. An elapsed budget never
//!   means "done": the abort-side waiter blocks on a condvar until actual
//!   retirement. Every registered closure is finite (a capacity push is
//!   released by `close_and_clear`), so the wait always terminates.

use std::sync::{Condvar, Mutex, PoisonError};

/// Hydrator lifecycle state for one transcript pump epoch.
struct Phase<T> {
    /// Producers registered (queued or running). Zero is idle.
    count: usize,
    /// Of those, non-terminal collectors (poll maps). The remainder are
    /// terminal finishes: a terminal claimant yields to every registered
    /// collector and to a terminal that registered ahead of it.
    collectors: usize,
    /// The live hydrator. `None` while a producer has claimed it, when the
    /// epoch is unbound, or after retirement.
    hydrator: Option<T>,
    /// Set once the hydrator is permanently finished: no further frames can
    /// be produced and the flusher may drain and exit.
    retired: bool,
}

/// The single synchronization point between every producer and the shutdown
/// decision for one pump/flusher pair.
pub(crate) struct ProducerGate<T> {
    state: Mutex<Phase<T>>,
    /// Notified on every completion: a claimer waiting for the hydrator, or
    /// the abort-side waiter blocking until retirement, wakes without polling.
    cv: Condvar,
}

impl<T> ProducerGate<T> {
    /// An unbound, idle epoch.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(Phase {
                count: 0,
                collectors: 0,
                hydrator: None,
                retired: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Phase<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Publish the live hydrator (first bind). Completes instantly when the
    /// epoch has already retired (the bind lost to close).
    pub(crate) fn publish(&self, hydrator: T) {
        let mut state = self.lock();
        if !state.retired {
            state.hydrator = Some(hydrator);
        }
    }

    /// Whether a live hydrator is currently in the slot (unclaimed, not
    /// retired).
    pub(crate) fn has_hydrator(&self) -> bool {
        self.lock().hydrator.is_some()
    }

    /// Register a non-terminal collector BEFORE its blocking closure is
    /// enqueued. Runs synchronously on the pump task, so a closure merely
    /// queued on the blocking pool is already visible to the abort decision.
    /// Returns false when the epoch had already retired, in which case the
    /// caller must not claim or complete a registration.
    pub(crate) fn register(&self) -> bool {
        let mut state = self.lock();
        if state.retired {
            return false;
        }
        state.count += 1;
        state.collectors += 1;
        true
    }

    /// Register a terminal finisher (cooperative finalise or abort residual).
    /// A terminal claimant yields to every registered collector and to any
    /// terminal registered ahead of it, so a finish can never steal a
    /// hydrator from a queued collect (c-ctxusage r9 item 2).
    pub(crate) fn register_terminal(&self) -> bool {
        let mut state = self.lock();
        if state.retired {
            return false;
        }
        state.count += 1;
        true
    }

    /// Whether at least one producer is registered (queued or running).
    pub(crate) fn producer_active(&self) -> bool {
        self.lock().count > 0
    }

    /// Claim the hydrator as a TERMINAL finisher. Yields to every registered
    /// collector and to a terminal registered earlier, blocking this
    /// (blocking) thread on a condvar — never a polled sleep or an elapsed
    /// budget. Returns `None` when the epoch is unbound or already retired.
    pub(crate) fn claim_terminal(&self) -> Option<T> {
        let mut state = self.lock();
        loop {
            if state.retired {
                return None;
            }
            // Other registered producers that must go first: any collector
            // (queued or running) OR another terminal ahead of this one.
            let other_terminals = state.count - state.collectors - 1;
            let must_yield =
                state.collectors > 0 || other_terminals > 0 || state.hydrator.is_none();
            if !must_yield {
                return state.hydrator.take();
            }
            // Unbound and alone: nobody will ever publish a hydrator.
            if state.count <= 1 && state.hydrator.is_none() {
                return None;
            }
            state = self.cv.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Claim as a non-terminal collector. Queued collectors are serialized:
    /// the hydrator, when present, belongs to the registered collector; when
    /// another producer owns it, block until it is restored or the epoch
    /// retires.
    pub(crate) fn claim(&self) -> Option<T> {
        let mut state = self.lock();
        loop {
            if state.retired {
                return None;
            }
            if let Some(hydrator) = state.hydrator.take() {
                return Some(hydrator);
            }
            // Unbound with nobody else holding it: no producer is coming to
            // publish one for THIS epoch; the caller's collect is a no-op.
            if state.count <= 1 {
                return None;
            }
            // Another registered producer owns the slot; wait for its
            // completion rather than a polled sleep or an elapsed budget.
            state = self.cv.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Return the hydrator after a non-terminal collect and drop the
    /// producer registration atomically.
    pub(crate) fn restore(&self, hydrator: T) {
        let mut state = self.lock();
        if !state.retired {
            state.hydrator = Some(hydrator);
        }
        state.count = state.count.saturating_sub(1);
        state.collectors = state.collectors.saturating_sub(1);
        self.cv.notify_all();
    }

    /// Drop a non-terminal registration WITHOUT returning a hydrator (an
    /// unbound collect, or a claim that lost the race to retirement).
    pub(crate) fn complete_empty(&self) {
        let mut state = self.lock();
        state.count = state.count.saturating_sub(1);
        state.collectors = state.collectors.saturating_sub(1);
        self.cv.notify_all();
    }

    /// Drop a TERMINAL registration that found the epoch already retired
    /// (another terminal finished first).
    pub(crate) fn complete_terminal(&self) {
        let mut state = self.lock();
        state.count = state.count.saturating_sub(1);
        self.cv.notify_all();
    }

    /// Discard the live hydrator unread (a foreign/cwd-mismatched bind)
    /// without registering a producer.
    pub(crate) fn discard_hydrator(&self) -> Option<T> {
        self.lock().hydrator.take()
    }

    /// Finish the registered producer and retire the epoch: the hydrator is
    /// not returned and no further frames may be produced. THE sole
    /// completion signal.
    pub(crate) fn retire(&self) {
        let mut state = self.lock();
        state.count = state.count.saturating_sub(1);
        state.hydrator = None;
        state.retired = true;
        self.cv.notify_all();
    }

    /// Retire with no producer registered (an unbound epoch whose abort
    /// residual still has to release the flusher).
    pub(crate) fn retire_idle(&self) {
        let mut state = self.lock();
        state.hydrator = None;
        state.retired = true;
        self.cv.notify_all();
    }

    /// Whether the epoch has actually retired.
    pub(crate) fn is_retired(&self) -> bool {
        self.lock().retired
    }

    /// Block this (off-runtime) thread until the epoch actually retires.
    /// Returns immediately if it already has. An elapsed budget is never
    /// completion: the wait is a condvar wait, and every registered producer
    /// is finite.
    pub(crate) fn wait_retired(&self) {
        let mut state = self.lock();
        while !state.retired {
            state = self.cv.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_queued_producer_blocks_the_abort_decision_until_retirement() {
        let gate = Arc::new(ProducerGate::<u32>::new());
        gate.publish(7);
        // The pump registers SYNCHRONOUSLY, before the closure is queued.
        gate.register();
        assert!(gate.producer_active());
        assert!(!gate.is_retired());

        // Abort-side waiter on a detached thread: must not return while the
        // closure is merely queued (the r9 item 2 race).
        let waiter_gate = Arc::clone(&gate);
        let waiter = std::thread::spawn(move || waiter_gate.wait_retired());
        std::thread::sleep(Duration::from_millis(30));
        assert!(!waiter.is_finished(), "waiter returned before retirement");

        // The queued closure runs, claims, finishes and retires.
        let claimed = gate.claim();
        assert_eq!(claimed, Some(7));
        gate.retire();
        waiter.join().expect("waiter thread");
        assert!(gate.is_retired());
        // Nothing can claim after retirement.
        assert!(gate.claim().is_none());
    }

    #[test]
    fn claimants_wait_for_the_hydrator_and_each_claim_runs_once() {
        let gate = Arc::new(ProducerGate::<u32>::new());
        gate.publish(1);
        gate.register(); // collect, queued
        gate.register(); // abort residual, queued behind it

        let first_gate = Arc::clone(&gate);
        let first = std::thread::spawn(move || {
            let h = first_gate.claim().expect("collect claims");
            std::thread::sleep(Duration::from_millis(20));
            // restore both returns the hydrator and drops the registration.
            first_gate.restore(h);
        });
        // Give the first claimant time to own the slot.
        std::thread::sleep(Duration::from_millis(10));
        let second_gate = Arc::clone(&gate);
        let second = std::thread::spawn(move || {
            let h = second_gate.claim().expect("residual claims after restore");
            second_gate.retire();
            h
        });
        first.join().expect("first");
        let finished_by = second.join().expect("second");
        assert_eq!(finished_by, 1);
        assert!(gate.is_retired());
        assert!(!gate.producer_active());
    }

    #[test]
    fn a_terminal_finish_yields_to_a_queued_collector() {
        // c-ctxusage r9 item 2 core scenario: cancel lands while the
        // collect closure is merely queued (registered, parked before
        // claim). The abort-side terminal must wait for the collector to
        // claim, run and restore; it never declares completion early.
        let gate = Arc::new(ProducerGate::<u32>::new());
        gate.publish(42);
        gate.register(); // queued collector (not yet claimed)
        gate.register_terminal(); // abort residual

        let terminal_gate = Arc::clone(&gate);
        let terminal = std::thread::spawn(move || terminal_gate.claim_terminal());
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            !terminal.is_finished(),
            "terminal stole from a queued collector"
        );

        // The collector claims and runs only now.
        assert_eq!(gate.claim(), Some(42));
        assert!(
            !terminal.is_finished(),
            "terminal must not finish mid-collect"
        );
        gate.restore(42);
        let claimed = terminal.join().expect("terminal thread");
        assert_eq!(claimed, Some(42), "terminal claims the restored hydrator");
        gate.retire();
        assert!(gate.is_retired());
    }
}
