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

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

/// Registration ticket: terminals finish strictly in registration order.
pub(crate) type TerminalTicket = usize;

/// Hydrator lifecycle state for one transcript pump epoch.
struct Phase<T> {
    /// Producers registered (queued or running). Zero is idle.
    count: usize,
    /// Of those, non-terminal collectors (poll maps). The remainder are
    /// terminal finishes: every terminal yields to every registered
    /// collector.
    collectors: usize,
    /// The live hydrator. `None` while a producer has claimed it, when the
    /// epoch is unbound, or after retirement.
    hydrator: Option<T>,
    /// Set once the hydrator is permanently finished: no further frames can
    /// be produced and the flusher may drain and exit.
    retired: bool,
    /// Tickets handed out to terminal registrations, in registration order.
    terminal_next: usize,
    /// Terminals still owed a claim decision, in registration order. The
    /// front ticket is the sole owner-elect once collectors drain; a later
    /// terminal waits for that owner to finish and then no-ops, so two
    /// registered terminals can never each wait on the other (c-ctxusage
    /// r10 item 1).
    terminal_queue: VecDeque<TerminalTicket>,
    /// Ticket of the terminal currently owning the hydrator, if any.
    terminal_owner: Option<TerminalTicket>,
    #[cfg(feature = "test-faults")]
    /// Test-only acknowledgement: invoked the moment a terminal claimant
    /// parks because an earlier-registered terminal is ahead of it in ticket
    /// order (it waits behind the elected owner). Lets an integration test
    /// prove the second terminal REGISTERED and reached its yield wait
    /// without inferring that from an elapsed delay. `None` in production.
    terminal_yield_hook: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync + 'static>>>,
    #[cfg(test)]
    /// Test-only acknowledgement: signalled (blocking rendezvous) the moment
    /// a thread enters a condvar wait, so a test can prove "parked" without a
    /// load-dependent sleep.
    wait_barrier: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
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
                terminal_next: 0,
                terminal_queue: VecDeque::new(),
                terminal_owner: None,
                #[cfg(feature = "test-faults")]
                terminal_yield_hook: std::sync::Mutex::new(None),
                #[cfg(test)]
                wait_barrier: std::sync::Mutex::new(None),
            }),
            cv: Condvar::new(),
        }
    }

    #[cfg(test)]
    /// Install the test-only condvar-wait acknowledgement channel.
    pub(crate) fn set_wait_barrier(&self, tx: std::sync::mpsc::SyncSender<()>) {
        *self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .wait_barrier
            .lock()
            .unwrap() = Some(tx);
    }

    #[cfg(feature = "test-faults")]
    /// Install the test-only acknowledgement fired when a terminal claimant
    /// parks behind an earlier-registered terminal (c-ctxusage r11 item 3).
    pub(crate) fn set_terminal_yield_hook(
        &self,
        hook: std::sync::Arc<dyn Fn() + Send + Sync + 'static>,
    ) {
        *self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .terminal_yield_hook
            .lock()
            .unwrap() = Some(hook);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Phase<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fire the test-only condvar-wait rendezvous while the gate lock is
    /// held, immediately before parking. Shared by every wait path so unit
    /// tests observe every park.
    #[cfg(test)]
    fn rendezvous_wait_barrier(guard: &Phase<T>) {
        let tx = guard
            .wait_barrier
            .lock()
            .unwrap()
            .as_ref()
            .map(std::clone::Clone::clone);
        if let Some(tx) = tx {
            // Blocks until the test acknowledges the parked thread.
            let _ = tx.send(());
        }
    }

    /// Condvar wait with a test-only acknowledgement fired the instant a
    /// thread parks. Inert in every non-test build.
    fn wait<'a>(&self, guard: MutexGuard<'a, Phase<T>>) -> MutexGuard<'a, Phase<T>> {
        #[cfg(test)]
        Self::rendezvous_wait_barrier(&guard);
        self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner)
    }

    /// Condvar wait on a TERMINAL-yield path (the claimant is a later
    /// terminal waiting behind the elected owner). Under `test-faults` it
    /// fires the terminal-yield acknowledgement while the gate mutex is held,
    /// immediately before parking: any observer reaction that lets the owner
    /// finish must first acquire this same mutex, so it cannot make the owner
    /// notify before this wait actually parks. Otherwise identical to
    /// [`Self::wait`].
    #[cfg(feature = "test-faults")]
    fn wait_terminal_yield<'a>(&self, guard: MutexGuard<'a, Phase<T>>) -> MutexGuard<'a, Phase<T>> {
        #[cfg(test)]
        Self::rendezvous_wait_barrier(&guard);
        if let Some(hook) = guard.terminal_yield_hook.lock().unwrap().clone() {
            hook();
        }
        self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(not(feature = "test-faults"))]
    fn wait_terminal_yield<'a>(&self, guard: MutexGuard<'a, Phase<T>>) -> MutexGuard<'a, Phase<T>> {
        self.wait(guard)
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
    /// Returns the registration ticket, or `None` when the epoch had already
    /// retired. Every terminal yields to every registered collector; among
    /// terminals the ticket order elects EXACTLY ONE owner, so two registered
    /// terminals can never each wait on the other (c-ctxusage r10 item 1).
    pub(crate) fn register_terminal(&self) -> Option<TerminalTicket> {
        let mut state = self.lock();
        if state.retired {
            return None;
        }
        let ticket = state.terminal_next;
        state.terminal_next += 1;
        state.count += 1;
        state.terminal_queue.push_back(ticket);
        Some(ticket)
    }

    /// Whether at least one producer is registered (queued or running).
    pub(crate) fn producer_active(&self) -> bool {
        self.lock().count > 0
    }

    /// Claim the hydrator as a TERMINAL finisher. Terminals finish in strict
    /// registration order: this call yields, on a condvar (never a polled
    /// sleep or an elapsed budget), while any collector is registered, while
    /// another terminal owns the hydrator, or while an earlier-registered
    /// terminal has yet to claim.
    ///
    /// The front terminal claims exactly once: it gets `Some(hydrator)` and
    /// becomes the owner; every later terminal is released with `None` once
    /// that owner finishes (retirement or an epoch boundary), so each frame
    /// is delivered once and no thread waits forever. `None` is also returned
    /// for an unbound epoch at the front of the queue (nobody will publish a
    /// hydrator); the caller then retires or boundary-finishes.
    pub(crate) fn claim_terminal(&self, ticket: TerminalTicket) -> Option<T> {
        let mut state = self.lock();
        loop {
            if state.retired {
                return None;
            }
            // Every registered collector — queued or running — goes first.
            if state.collectors > 0 {
                state = self.wait(state);
                continue;
            }
            // The elected owner is finishing: no-op once it is done.
            if state.terminal_owner.is_some() {
                state = self.wait_terminal_yield(state);
                continue;
            }
            // Strict registration order among the queued terminals.
            if state.terminal_queue.front() != Some(&ticket) {
                state = self.wait_terminal_yield(state);
                continue;
            }
            // My turn, and no collector or owner stands ahead.
            state.terminal_queue.pop_front();
            match state.hydrator.take() {
                Some(hydrator) => {
                    state.terminal_owner = Some(ticket);
                    return Some(hydrator);
                }
                None => {
                    // Unbound with every other registration behind me (they
                    // are later terminals, which never publish hydrators):
                    // nobody is coming to bind THIS epoch.
                    return None;
                }
            }
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
            state = self.wait(state);
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
    pub(crate) fn complete_terminal(&self, ticket: TerminalTicket) {
        let mut state = self.lock();
        state.terminal_queue.retain(|queued| *queued != ticket);
        state.count = state.count.saturating_sub(1);
        self.cv.notify_all();
    }

    /// Finish a TERMINAL producer at an EPOCH BOUNDARY rather than the end of
    /// the pump: the hydrator is not returned and the registration drops, but
    /// the epoch is not retired, so a later bind can publish a new hydrator
    /// (demote/rebind within one promoter lifetime).
    pub(crate) fn finish_boundary(&self) {
        let mut state = self.lock();
        state.hydrator = None;
        state.terminal_owner = None;
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
        state.terminal_owner = None;
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
            state = self.wait(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    /// Every test wait is bounded: a deadlock under test must fail loudly and
    /// promptly, never hang the test binary (c-ctxusage r11 item 3).
    const TEST_WAIT: Duration = Duration::from_secs(30);

    fn recv_bounded<T: Send>(rx: &std::sync::mpsc::Receiver<T>, what: &str) -> T {
        rx.recv_timeout(TEST_WAIT)
            .unwrap_or_else(|err| panic!("{what} within {TEST_WAIT:?}: {err:?}"))
    }

    fn join_bounded<T: Send + 'static>(handle: std::thread::JoinHandle<T>, what: &str) -> T {
        let (tx, rx) = sync_channel(1);
        // The handle is moved onto a watcher so a join that would block
        // forever is observed through the bounded channel instead. The
        // JoinHandle is dropped on purpose: the watcher thread runs on and is
        // reaped by process exit if the bounded wait ever times out.
        let _watcher = std::thread::spawn(move || {
            let _ = tx.send(handle.join());
        });
        match rx.recv_timeout(TEST_WAIT) {
            Ok(Ok(value)) => value,
            Ok(Err(payload)) => std::panic::resume_unwind(payload),
            Err(err) => panic!("{what} within {TEST_WAIT:?}: {err:?}"),
        }
        // The watcher exits right after sending; on a timeout panic it
        // detaches with the hung thread and process exit reaps it.
    }

    #[test]
    fn a_queued_producer_blocks_the_abort_decision_until_retirement() {
        let gate = Arc::new(ProducerGate::<u32>::new());
        gate.publish(7);
        // The pump registers SYNCHRONOUSLY, before the closure is queued.
        gate.register();
        assert!(gate.producer_active());
        assert!(!gate.is_retired());

        // The barrier proves the abort-side waiter is actually PARKED in the
        // condvar (an acknowledgement, not a load-dependent settling sleep).
        let (wait_tx, wait_rx) = sync_channel(0);
        gate.set_wait_barrier(wait_tx);
        let waiter_gate = Arc::clone(&gate);
        let waiter = std::thread::spawn(move || waiter_gate.wait_retired());
        recv_bounded(&wait_rx, "waiter never entered the retirement wait");

        // The queued closure runs, claims, finishes and retires.
        let claimed = gate.claim();
        assert_eq!(claimed, Some(7));
        gate.retire();
        join_bounded(waiter, "waiter thread never returned from wait_retired");
        assert!(gate.is_retired());
        // Nothing can claim after retirement.
        assert!(gate.claim().is_none());
    }

    #[test]
    fn claimants_wait_for_the_hydrator_and_each_claim_runs_once() {
        let gate = Arc::new(ProducerGate::<u32>::new());
        gate.publish(1);
        gate.register(); // collect, queued
        gate.register(); // residual, queued behind it

        // A claims the hydrator, then parks holding it until the test opens
        // its release rendezvous.
        let (got_tx, got_rx) = sync_channel(0);
        let (release_tx, release_rx) = sync_channel(0);
        let first_gate = Arc::clone(&gate);
        let first = std::thread::spawn(move || {
            let h = first_gate.claim().expect("collect claims");
            got_tx.send(()).expect("acknowledge ownership");
            release_rx
                .recv_timeout(TEST_WAIT)
                .expect("release never arrived within bound");
            first_gate.restore(h);
        });
        recv_bounded(&got_rx, "first claimant never owned the slot");

        // B must park waiting for the owned hydrator; the wait barrier makes
        // that state observable without a sleep.
        let (wait_tx, wait_rx) = sync_channel(0);
        gate.set_wait_barrier(wait_tx);
        let second_gate = Arc::clone(&gate);
        let second = std::thread::spawn(move || {
            let h = second_gate.claim().expect("residual claims after restore");
            second_gate.retire();
            h
        });
        recv_bounded(&wait_rx, "second claimant never parked on the slot");

        release_tx.send(()).expect("release the first claimant");
        join_bounded(first, "first claimant thread never finished");
        let finished_by = join_bounded(second, "second claimant thread never finished");
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
        let ticket = gate.register_terminal().expect("terminal registers");

        let (wait_tx, wait_rx) = sync_channel(0);
        gate.set_wait_barrier(wait_tx);
        let terminal_gate = Arc::clone(&gate);
        let terminal = std::thread::spawn(move || terminal_gate.claim_terminal(ticket));
        // Hard acknowledgement: the terminal is parked yielding, rather than
        // a "still running after N ms" timing guess.
        recv_bounded(&wait_rx, "terminal never parked to yield");

        // The collector claims and runs only now.
        assert_eq!(gate.claim(), Some(42));
        gate.restore(42);
        let claimed = join_bounded(terminal, "terminal thread never claimed after restore");
        assert_eq!(claimed, Some(42), "terminal claims the restored hydrator");
        gate.retire();
        assert!(gate.is_retired());
    }

    #[test]
    fn two_terminals_registered_before_either_claims_elect_one_owner() {
        // c-ctxusage r10 item 1: the cooperative finalise registers a
        // terminal and its closure sits queued BEFORE claiming; close then
        // aborts and the residual thread registers a SECOND terminal. The old
        // "other_terminals > 0" rule made BOTH wait on each other forever.
        // Tickets elect the earlier terminal owner exactly once; the later
        // one no-ops after retirement.
        let gate = Arc::new(ProducerGate::<u32>::new());
        gate.publish(99);
        // Both registered while neither closure has run.
        let ticket_a = gate.register_terminal().expect("A");
        let ticket_b = gate.register_terminal().expect("B");

        // The residual B reaches its claim FIRST (A is still merely queued)
        // and must park because A is ahead in ticket order.
        let (wait_tx, wait_rx) = sync_channel(0);
        gate.set_wait_barrier(wait_tx);
        let b_gate = Arc::clone(&gate);
        let b = std::thread::spawn(move || {
            // Mirror run_terminal_finalise exactly, including the retired
            // no-op's registration drop.
            match b_gate.claim_terminal(ticket_b) {
                Some(hydrator) => {
                    b_gate.retire();
                    Some(hydrator)
                }
                None => {
                    if b_gate.is_retired() {
                        b_gate.complete_terminal(ticket_b);
                    } else {
                        b_gate.retire();
                    }
                    None
                }
            }
        });
        recv_bounded(&wait_rx, "B never parked while A was ahead in ticket order");

        // A now runs: it is the sole owner and takes the hydrator.
        let a_gate = Arc::clone(&gate);
        let a = std::thread::spawn(move || {
            let Some(hydrator) = a_gate.claim_terminal(ticket_a) else {
                panic!("the earlier terminal must be the owner, never a no-op");
            };
            a_gate.retire();
            hydrator
        });
        assert_eq!(
            join_bounded(a, "owner terminal A never finished"),
            99,
            "A owns the hydrator"
        );

        // B wakes retired and returns the no-op: exactly-once delivery.
        let b_claim = join_bounded(b, "later terminal B never returned from its claim");
        assert_eq!(b_claim, None, "the later terminal never finishes twice");
        assert!(gate.is_retired());
        assert!(!gate.producer_active());
        // A terminal arriving after retirement does not even register.
        assert!(gate.register_terminal().is_none());
    }

    #[test]
    fn a_later_terminal_noops_after_an_epoch_boundary_then_close_retires() {
        // In-life demote/rebind: the owner finishes at a boundary (no
        // retirement); a later terminal then finds the epoch unbound and its
        // caller boundary/retire choice releases it.
        let gate = ProducerGate::<u32>::new();
        gate.publish(1);
        let ticket_a = gate.register_terminal().expect("A");
        let ticket_b = gate.register_terminal().expect("B");
        assert_eq!(gate.claim_terminal(ticket_a), Some(1));
        // A boundary-finishes (in-life rebind): owner clears but the epoch
        // lives; B is now the front with no hydrator.
        gate.finish_boundary();
        assert!(!gate.is_retired());
        assert_eq!(gate.claim_terminal(ticket_b), None, "unbound front no-ops");
        // B's abort caller retires to release the flusher.
        gate.retire();
        assert!(gate.is_retired());
        assert!(!gate.producer_active());
    }

    #[test]
    fn an_epoch_boundary_finish_allows_a_later_rebind_but_a_close_does_not() {
        // In-life demote/rebind finalise: the hydrator is dropped but the
        // epoch stays alive, so a following publish binds a new hydrator.
        let gate = ProducerGate::<u32>::new();
        gate.publish(1);
        assert!(gate.register_terminal().is_some());
        let hydrator = gate.claim_terminal(0).expect("boundary claims");
        assert_eq!(hydrator, 1);
        gate.finish_boundary();
        assert!(!gate.is_retired());
        assert!(!gate.producer_active());
        gate.publish(2);
        assert!(gate.has_hydrator());

        // The terminal close finish retires for good: publish is a no-op.
        let ticket = gate.register_terminal().expect("close terminal");
        assert_eq!(gate.claim_terminal(ticket), Some(2));
        gate.retire();
        gate.publish(3);
        assert!(!gate.has_hydrator());
        assert!(gate.register_terminal().is_none());
    }
}
