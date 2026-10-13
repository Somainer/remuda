//! Deterministic fault-injection seam for the transcript close/abort tests
//! (c-ctxusage r8 item 6).
//!
//! The r7 seam used sibling marker files (`.remuda-test-collect-gate` /
//! `.remuda-test-skip-coop-finalise`), which made production code read the
//! filesystem for test behaviour: a stray file could stall a collect or skip
//! a finalise in a release build. This module is the replacement:
//!
//! * the [`FaultState`] plumbing compiles into every build but is completely
//!   inert until a test injects a handle;
//! * a handle is constructible only behind the `test-faults` cargo feature,
//!   which is enabled solely by dev-dependencies, never by release binaries;
//! * synchronisation is barrier/ack based — blocking phases signal an
//!   "entered" acknowledgement and wait on a gate the test opens, so tests
//!   never sleep-poll or rely on load-dependent timing.

#[cfg(feature = "test-faults")]
use std::sync::Arc;
use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

/// Which blocking phase the test wants to park.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The first `collect_pump` / blocking transcript collect.
    Pump,
    /// The cooperative finalise (`finish_collect` / final `finish()`).
    Finish,
    /// A blocking closure that is QUEUED/running but has not yet claimed the
    /// hydrator (c-ctxusage r9 item 2).
    Queued,
}

/// Inert-by-default fault state held by the transcript pumps.
pub(crate) struct FaultState {
    pump_armed: AtomicBool,
    finish_armed: AtomicBool,
    queued_armed: AtomicBool,
    /// Arms the ONE-SHOT gate that parks the first registered terminal
    /// finaliser before its claim (c-ctxusage r10 item 1).
    terminal_armed: AtomicBool,
    pump_entered: Mutex<bool>,
    pump_entered_cv: Condvar,
    finish_entered: Mutex<bool>,
    finish_entered_cv: Condvar,
    queued_entered: Mutex<bool>,
    queued_entered_cv: Condvar,
    terminal_entered: Mutex<bool>,
    terminal_entered_cv: Condvar,
    /// The registration ticket the parked cooperative terminal actually got.
    /// A demote/rebind before close consumes ticket 0 for its in-life boundary
    /// finish, so the close-time cooperative terminal registers a LATER
    /// ticket; the gate selects on the cooperative flag and records whichever
    /// ticket that terminal holds rather than hardcoding zero (c-ctxusage
    /// r13 item 7).
    cooperative_ticket: Mutex<Option<usize>>,
    cooperative_ticket_cv: Condvar,
    gate_open: Mutex<bool>,
    gate_cv: Condvar,
    abort_residual: Mutex<bool>,
    abort_residual_cv: Condvar,
    #[cfg(feature = "test-faults")]
    /// Acknowledged once the abort-residual terminal has REGISTERED and parked
    /// in its claim behind the elected owner terminal - hard proof the second
    /// terminal exists, instead of inferring it from an elapsed delay
    /// (c-ctxusage r11 item 3).
    abort_terminal_waiting: Mutex<bool>,
    #[cfg(feature = "test-faults")]
    abort_terminal_waiting_cv: Condvar,
    flusher_exited: Mutex<bool>,
    flusher_exited_cv: Condvar,
    /// Acknowledged once the streaming flusher has drained and is parked in
    /// `notified().await` (c-ctxusage r9 item 1): lets a test prove the
    /// residual batch lands while the flusher is idle/asleep.
    flusher_idle: Mutex<bool>,
    flusher_idle_cv: Condvar,
    /// Acknowledged once the promoter task (with its flusher and gate) has
    /// actually spawned — the state an unbound close must not deadlock
    /// against. `wait_control` returns Ok immediately for a plain shell, so it
    /// cannot prove this (c-ctxusage r10 item 5).
    promoter_started: Mutex<bool>,
    promoter_started_cv: Condvar,
    /// Total promotion poller spawns across runs (c-ctxusage r20 item 2):
    /// unlike the one-shot `promoter_started` bool this counts run 1 AND the
    /// resumed run 2, so a test can prove run 2's poller spawned while the
    /// predecessor was still parked in finalise. Compiled only with
    /// `test-faults` (zero cost in production, c-ctxusage r21 item 2).
    #[cfg(feature = "test-faults")]
    promoter_starts: Mutex<u64>,
    #[cfg(feature = "test-faults")]
    promoter_starts_cv: Condvar,
    /// NORMAL-sample gate (c-ctxusage r21 item 3): while armed the poller may
    /// not run a normal process sample, but close/drain/shutdown wakeups still
    /// pass. Async-aware (a `watch` future), so parking never blocks a runtime
    /// worker. Release sets the watch true.
    #[cfg(feature = "test-faults")]
    normal_gate_armed: AtomicBool,
    #[cfg(feature = "test-faults")]
    normal_gate_tx: tokio::sync::watch::Sender<bool>,
    #[cfg(feature = "test-faults")]
    normal_parked: Mutex<u64>,
    #[cfg(feature = "test-faults")]
    normal_parked_cv: Condvar,
    /// Sample-entry hold (c-ctxusage r22 item 3): when armed, a NORMAL sample
    /// blocks the moment it begins sampling (after the normal-sample gate, so a
    /// correctly-placed gate parks first and never reaches this). Lets a test
    /// prove the gate stops the sample itself, not just the next tick.
    #[cfg(feature = "test-faults")]
    sample_hold_armed: AtomicBool,
    #[cfg(feature = "test-faults")]
    sample_hold_tx: tokio::sync::watch::Sender<bool>,
    #[cfg(feature = "test-faults")]
    sample_hold_entered: Mutex<bool>,
    #[cfg(feature = "test-faults")]
    sample_hold_entered_cv: Condvar,
    skip_cooperative_finalise: AtomicBool,
    /// Overrides the driver's bounded cooperative-close wait so an abort test
    /// crosses the bound deterministically without the production 15 s / 2 s
    /// wait. Milliseconds; 0 means "use the production timeout".
    close_timeout_ms: AtomicU64,
    /// Overrides the bounded pending queue capacity so a test can make a
    /// normal-sized replay batch exceed it. 0 means production capacity.
    pending_capacity: AtomicU64,
    /// Overrides the native stop-phase grace ([`AGENT_EXIT_GRACE`]) so a test
    /// that models a writer outliving Ctrl-C reaches forced termination
    /// deterministically without the production 2 s wait (c-ctxusage r13
    /// item 1). Milliseconds; 0 means the production grace.
    agent_exit_grace_ms: AtomicU64,
    /// Overrides the promoted-close poller handoff barrier bound
    /// (PROMOTER_CLOSE_BARRIER) so a test that exercises a missed/foreign
    /// drain gets the retryable error fast (P1, c-ctxusage r16 item 6).
    /// Milliseconds; 0 means the production bound.
    close_barrier_timeout_ms: AtomicU64,
    /// Remaining close calls whose native stop-phase ownership probe must
    /// fail, modelling Herdr refusing `session.snapshot` during its shutdown
    /// window (c-ctxusage r13 item 2): one decremented failure per close
    /// attempt, so a retried close probes cleanly.
    close_probe_failures: AtomicU64,
    /// Remaining shell-pty SPAWN attempts that must fail right after launch
    /// materialization and before the child exists — the
    /// materialize/PTY-open/spawn_command failure shape (c-ctxusage r20
    /// item 1). Consumed inside `spawn_at`, so a resumed spawn that consumes
    /// it fails while the predecessor run is left untouched.
    spawn_failures: AtomicU64,
    /// Arms the one-shot gate that parks the first close between its final
    /// drain and its reap/terminal emission (c-ctxusage r14 item 5).
    close_tail_armed: AtomicBool,
    /// Acknowledged when the armed close reaches the park point.
    close_tail_entered: Mutex<bool>,
    close_tail_entered_cv: Condvar,
    /// Async release the parked close waits for, installed BEFORE the
    /// entered acknowledgement so a release that lands before the parked
    /// future is polled is never lost (a bare `Notify::notify_waiters`
    /// would be — c-ctxusage r15 item 9). `Some` while an armed close is
    /// parked; the test's release takes and fires the oneshot.
    close_tail_gate: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Number of close() calls that have reached the close-phase entry
    /// (signalled BEFORE acquiring the owner), so a test can prove a second
    /// close is genuinely in flight without any timing inference.
    close_phase_calls: Mutex<u64>,
    close_phase_calls_cv: Condvar,
    /// Remaining promoted-close attempts that must report the native writer
    /// STILL LIVE after the stop ladder (P1, c-ctxusage r16 item 5): one
    /// decremented failure per close attempt, so the first close returns the
    /// retryable `WriterStillLive` error without retiring the hydrator, and a
    /// retried close runs the real verification and completes.
    promoted_stop_incomplete_attempts: AtomicU64,
    /// Acknowledged when a promoted close is parked at the poller handoff
    /// barrier, waiting for an in-flight normal poll to finish (P1,
    /// c-ctxusage r16 item 6).
    close_barrier_waiting: Mutex<bool>,
    close_barrier_waiting_cv: Condvar,
    /// Number of times a promoted close ARMED the poller handoff barrier.
    /// Concurrent closes are serialized on the poller slot, so a second
    /// close must not arm until the first handoff is resolved (c-ctxusage
    /// r17 item 3).
    close_barrier_arms: Mutex<u64>,
    close_barrier_arms_cv: Condvar,
    /// Number of promotion polls that established a hydrator (a transcript
    /// BIND), across runs. A same-driver resume test waits for the SECOND run's
    /// bind while the predecessor poller is held parked, so the bind provably
    /// lands before predecessor retirement clears the shared detection slots
    /// (c-ctxusage r19 item 1).
    promoter_binds: Mutex<u64>,
    promoter_binds_cv: Condvar,
}

/// A parked phase must never wait forever on a test that forgot to release:
/// bound the gate so a buggy test fails loudly instead of hanging the binary.
const GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

impl FaultState {
    #[cfg(feature = "test-faults")]
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            pump_armed: AtomicBool::new(false),
            finish_armed: AtomicBool::new(false),
            queued_armed: AtomicBool::new(false),
            terminal_armed: AtomicBool::new(false),
            pump_entered: Mutex::new(false),
            pump_entered_cv: Condvar::new(),
            finish_entered: Mutex::new(false),
            finish_entered_cv: Condvar::new(),
            queued_entered: Mutex::new(false),
            queued_entered_cv: Condvar::new(),
            terminal_entered: Mutex::new(false),
            terminal_entered_cv: Condvar::new(),
            cooperative_ticket: Mutex::new(None),
            cooperative_ticket_cv: Condvar::new(),
            gate_open: Mutex::new(false),
            gate_cv: Condvar::new(),
            abort_residual: Mutex::new(false),
            abort_residual_cv: Condvar::new(),
            abort_terminal_waiting: Mutex::new(false),
            abort_terminal_waiting_cv: Condvar::new(),
            flusher_exited: Mutex::new(false),
            flusher_exited_cv: Condvar::new(),
            flusher_idle: Mutex::new(false),
            flusher_idle_cv: Condvar::new(),
            promoter_started: Mutex::new(false),
            promoter_started_cv: Condvar::new(),
            #[cfg(feature = "test-faults")]
            promoter_starts: Mutex::new(0),
            #[cfg(feature = "test-faults")]
            promoter_starts_cv: Condvar::new(),
            #[cfg(feature = "test-faults")]
            normal_gate_armed: AtomicBool::new(false),
            #[cfg(feature = "test-faults")]
            normal_gate_tx: tokio::sync::watch::channel(false).0,
            #[cfg(feature = "test-faults")]
            normal_parked: Mutex::new(0),
            #[cfg(feature = "test-faults")]
            normal_parked_cv: Condvar::new(),
            #[cfg(feature = "test-faults")]
            sample_hold_armed: AtomicBool::new(false),
            #[cfg(feature = "test-faults")]
            sample_hold_tx: tokio::sync::watch::channel(false).0,
            #[cfg(feature = "test-faults")]
            sample_hold_entered: Mutex::new(false),
            #[cfg(feature = "test-faults")]
            sample_hold_entered_cv: Condvar::new(),
            skip_cooperative_finalise: AtomicBool::new(false),
            close_timeout_ms: AtomicU64::new(0),
            pending_capacity: AtomicU64::new(0),
            agent_exit_grace_ms: AtomicU64::new(0),
            close_barrier_timeout_ms: AtomicU64::new(0),
            close_probe_failures: AtomicU64::new(0),
            spawn_failures: AtomicU64::new(0),
            close_tail_armed: AtomicBool::new(false),
            close_tail_entered: Mutex::new(false),
            close_tail_entered_cv: Condvar::new(),
            close_tail_gate: Mutex::new(None),
            close_phase_calls: Mutex::new(0),
            close_phase_calls_cv: Condvar::new(),
            promoted_stop_incomplete_attempts: AtomicU64::new(0),
            close_barrier_waiting: Mutex::new(false),
            close_barrier_waiting_cv: Condvar::new(),
            close_barrier_arms: Mutex::new(0),
            close_barrier_arms_cv: Condvar::new(),
            promoter_binds: Mutex::new(0),
            promoter_binds_cv: Condvar::new(),
        })
    }

    /// Record one close() call reaching the close-phase entry. Signalled
    /// BEFORE the close-phase owner is acquired, so it proves an overlapping
    /// close is in flight even while it is queued behind the first one
    /// (c-ctxusage r14 item 5).
    pub(crate) fn note_close_phase_called(&self) {
        let mut calls = self
            .close_phase_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *calls += 1;
        self.close_phase_calls_cv.notify_all();
    }

    /// Park the first close after its final drain and before reap/terminal
    /// emission. Inert unless armed; bounded so a test that arms but never
    /// releases cannot wedge the binary.
    pub(crate) async fn await_close_tail_gate(&self) {
        if !self.close_tail_armed.swap(false, Ordering::SeqCst) {
            return;
        }
        // Install the one-shot release target FIRST (c-ctxusage r15 item 9):
        // a release that races this park and lands before it is polled is
        // delivered through the channel, whereas a Notify wake emitted with
        // no registered waiter would be silently lost.
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self
            .close_tail_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        // Only now publish the entered acknowledgement the test waits on
        // before releasing.
        *self
            .close_tail_entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.close_tail_entered_cv.notify_all();
        // The 120 s bound turns a forgotten release into a loud test failure
        // instead of a hang; a racing release is already buffered in rx.
        match tokio::time::timeout(Duration::from_secs(120), rx).await {
            Ok(Ok(())) => {}
            // Timed out, or the release half dropped (a second armed close
            // replaced it): proceed either way.
            _ => tracing::debug!("close-tail gate timed out; proceeding"),
        }
        // Park is over: drop any stale sender before the next arm.
        *self
            .close_tail_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Called from INSIDE the blocking phase (a spawn_blocking closure).
    /// Signals the entered acknowledgement and then parks until the test opens
    /// the gate. Never touches an async runtime.
    fn enter(&self, phase: Phase) {
        let (armed, entered, cv) = match phase {
            Phase::Pump => (&self.pump_armed, &self.pump_entered, &self.pump_entered_cv),
            Phase::Finish => (
                &self.finish_armed,
                &self.finish_entered,
                &self.finish_entered_cv,
            ),
            Phase::Queued => (
                &self.queued_armed,
                &self.queued_entered,
                &self.queued_entered_cv,
            ),
        };
        if !armed.load(Ordering::SeqCst) {
            return;
        }
        *entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        cv.notify_all();
        let mut open = self
            .gate_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*open {
            let (guard, timeout) = self
                .gate_cv
                .wait_timeout(open, GATE_WAIT)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            open = guard;
            if timeout.timed_out() {
                // A test that armed a gate must release it; proceed so the
                // process cannot be wedged permanently by a stale arm.
                break;
            }
        }
    }

    /// Park the first blocking transcript collect before it polls the tail.
    pub(crate) fn pump_collect_entered(&self) {
        self.enter(Phase::Pump);
    }

    /// Park the cooperative finalise before it runs `finish()`.
    pub(crate) fn cooperative_finish_entered(&self) {
        self.enter(Phase::Finish);
    }

    /// Park a blocking closure the moment it starts, BEFORE it claims the
    /// hydrator (c-ctxusage r9 item 2). Registration already happened, so the
    /// abort decision must see an active producer.
    pub(crate) fn blocking_closure_queued(&self) {
        self.enter(Phase::Queued);
    }

    /// Park the cooperative terminal finaliser (the close-time owner) BEFORE
    /// it claims the hydrator (c-ctxusage r10 item 1).
    ///
    /// The gate admits the cooperative terminal by its EXPLICIT `cooperative`
    /// flag and captures whichever registration ticket it holds - never by a
    /// hardcoded ticket zero. The close-time finalise is the only cooperative
    /// caller: the abort residual passes false and an in-life demote/rebind
    /// boundary finish passes `final_close=false` too. That boundary terminal
    /// registers FIRST and consumes ticket 0 when a demote/rebind happens
    /// before close, leaving the cooperative terminal on a later ticket;
    /// selecting `cooperative && ticket == 0` (r12 item 3's belt-and-braces
    /// rule) then failed to ever park it (c-ctxusage r13 item 7). The flag
    /// alone still keeps the abort thread out even when it executes first - it
    /// reaches the gate claim and parks there instead - so the r12 guarantee
    /// is preserved.
    pub(crate) fn terminal_finalise_queued(
        &self,
        ticket: crate::producer_gate::TerminalTicket,
        cooperative: bool,
    ) {
        if !self.terminal_armed.load(Ordering::SeqCst) || !cooperative {
            return;
        }
        let mut guard = self
            .terminal_entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = true;
        self.terminal_entered_cv.notify_all();
        drop(guard);
        *self
            .cooperative_ticket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ticket);
        self.cooperative_ticket_cv.notify_all();
        let mut open = self
            .gate_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*open {
            let (next_open, timeout) = self
                .gate_cv
                .wait_timeout(open, GATE_WAIT)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            open = next_open;
            if timeout.timed_out() {
                break;
            }
        }
    }

    /// When true, the cooperative reading finalise is a no-op so the abort
    /// guard is the sole rescuer (models close aborting past its bound).
    pub(crate) fn skip_cooperative_finalise(&self) -> bool {
        self.skip_cooperative_finalise.load(Ordering::SeqCst)
    }

    /// Acknowledge that the abort guard's detached residual-finish thread has
    /// started, so a test can prove the timeout-driven abort actually happened
    /// (rather than the cooperative path publishing before close).
    pub(crate) fn note_abort_residual(&self) {
        *self
            .abort_residual
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.abort_residual_cv.notify_all();
    }

    /// Acknowledge that the abort residual's terminal registered and is parked
    /// in its claim, waiting behind the elected owner terminal (c-ctxusage
    /// r11 item 3). Fired from the gate's terminal-yield hook.
    #[cfg(feature = "test-faults")]
    pub(crate) fn note_abort_terminal_waiting(&self) {
        *self
            .abort_terminal_waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.abort_terminal_waiting_cv.notify_all();
    }

    /// The injected cooperative-close timeout override, if a test armed one.
    pub(crate) fn close_timeout(&self) -> Option<Duration> {
        match self.close_timeout_ms.load(Ordering::SeqCst) {
            0 => None,
            millis => Some(Duration::from_millis(millis)),
        }
    }

    /// The injected promoted-close barrier timeout override, if armed.
    pub(crate) fn close_barrier_timeout(&self) -> Option<Duration> {
        match self.close_barrier_timeout_ms.load(Ordering::SeqCst) {
            0 => None,
            millis => Some(Duration::from_millis(millis)),
        }
    }

    /// Consume one armed promoted stop-incomplete attempt (P1,
    /// c-ctxusage r16 item 5). Returns true while the current promoted close
    /// must synthesize a writer-that-survived-the-ladder outcome.
    pub(crate) fn take_promoted_stop_incomplete(&self) -> bool {
        self.promoted_stop_incomplete_attempts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                // An explicit block: `then_some(n - 1)` would evaluate `n - 1`
                // eagerly even when n is 0 and panic on underflow.
                if n > 0 { Some(n - 1) } else { None }
            })
            .is_ok_and(|previous| previous > 0)
    }

    /// Whether the injected pump gate is CURRENTLY holding a blocking collect
    /// parked (entered, not yet released). The promoted close barrier uses
    /// this to tell a wedged-gate collect (the abort-rescue path proceeds and
    /// aborts the poller after the ladder) from a drain the poller never
    /// observes (a foreign-drain defect, which stays retryable) — P1,
    /// c-ctxusage r16 item 6.
    pub(crate) fn pump_gate_holds(&self) -> bool {
        let entered = *self
            .pump_entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let open = *self
            .gate_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entered && !open
    }

    /// Note that a promoted close is parked at the poller handoff barrier
    /// (P1, c-ctxusage r16 item 6).
    pub(crate) fn note_close_barrier_waiting(&self) {
        let mut waiting = self
            .close_barrier_waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *waiting = true;
        self.close_barrier_waiting_cv.notify_all();
    }

    /// Note one promoted close arming the poller handoff barrier. The
    /// serialization test waits on this counter to prove a concurrent close
    /// does not arm a second barrier while the first handoff is unresolved
    /// (c-ctxusage r17 item 3).
    pub(crate) fn note_close_barrier_armed(&self) {
        let mut arms = self
            .close_barrier_arms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *arms += 1;
        self.close_barrier_arms_cv.notify_all();
    }

    /// Record that a promotion poll established a hydrator (a transcript
    /// bind); wakes [`Self::wait_promoter_binds`] (c-ctxusage r19 item 1).
    pub(crate) fn note_promoter_bind(&self) {
        let mut binds = self
            .promoter_binds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *binds += 1;
        self.promoter_binds_cv.notify_all();
    }

    /// Acknowledge that the streaming flusher exited. A test holding a
    /// producer past close uses this to prove the flusher did not quit while
    /// final frames were still pending.
    pub(crate) fn note_flusher_exit(&self) {
        *self
            .flusher_exited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.flusher_exited_cv.notify_all();
    }

    /// Acknowledge that the flusher has drained the queue and is parked in
    /// `notified().await` — idle, waiting solely on the queue-integrated
    /// wake (c-ctxusage r9 item 1).
    pub(crate) fn note_flusher_idle(&self) {
        *self
            .flusher_idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.flusher_idle_cv.notify_all();
    }

    /// Acknowledge that the promoter task spawned with its flusher/gate
    /// handshake in place (c-ctxusage r10 item 5).
    pub(crate) fn note_promoter_started(&self) {
        *self
            .promoter_started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.promoter_started_cv.notify_all();
        // Per-run spawn count (c-ctxusage r20 item 2): test-faults builds only.
        #[cfg(feature = "test-faults")]
        {
            let mut starts = self
                .promoter_starts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *starts += 1;
            self.promoter_starts_cv.notify_all();
        }
    }

    /// Bounded wait for `n` promotion poller spawns (c-ctxusage r20 item 2);
    /// called from the driver (which holds `Arc<FaultState>`). Compiled only
    /// with `test-faults` (c-ctxusage r21 item 2).
    #[cfg(feature = "test-faults")]
    pub(crate) fn wait_promoter_starts_state(&self, n: u64, timeout: Duration) -> bool {
        let guard = self
            .promoter_starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, _) = self
            .promoter_starts_cv
            .wait_timeout_while(guard, timeout, |starts| *starts < n)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard >= n
    }

    /// A receiver on the normal-sample release gate (c-ctxusage r21 item 3).
    /// The poll loop awaits this (alongside shutdown/drain) only while the
    /// gate is armed.
    #[cfg(feature = "test-faults")]
    pub(crate) fn normal_gate_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.normal_gate_tx.subscribe()
    }

    /// Whether the normal-sample gate currently parks the poll loop.
    #[cfg(feature = "test-faults")]
    pub(crate) fn normal_gate_is_armed(&self) -> bool {
        self.normal_gate_armed.load(Ordering::SeqCst)
    }

    /// Count one normal-sample park and acknowledge it (c-ctxusage r21
    /// item 3): a test waits for this before issuing the close so it knows
    /// both pollers are holding normal samples.
    #[cfg(feature = "test-faults")]
    pub(crate) fn note_normal_sample_parked(&self) {
        let mut parked = self
            .normal_parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *parked += 1;
        self.normal_parked_cv.notify_all();
    }

    /// Release the normal-sample gate and disarm it (c-ctxusage r21 item 3):
    /// every parked poller resumes normal sampling.
    #[cfg(feature = "test-faults")]
    pub(crate) fn release_normal_samples_state(&self) {
        self.normal_gate_armed.store(false, Ordering::SeqCst);
        let _ = self.normal_gate_tx.send(true);
    }

    /// Receiver on the sample-entry hold gate (c-ctxusage r22 item 3).
    #[cfg(feature = "test-faults")]
    pub(crate) fn sample_hold_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.sample_hold_tx.subscribe()
    }

    /// Whether a normal sample may START: false while the sample-entry hold is
    /// armed (c-ctxusage r22 item 3).
    #[cfg(feature = "test-faults")]
    pub(crate) fn sample_hold_is_armed(&self) -> bool {
        self.sample_hold_armed.load(Ordering::SeqCst)
    }

    /// Acknowledge a normal sample entered (reached the start of sampling) and
    /// is parked on the entry hold (c-ctxusage r22 item 3).
    #[cfg(feature = "test-faults")]
    pub(crate) fn note_sample_hold_entered(&self) {
        *self
            .sample_hold_entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.sample_hold_entered_cv.notify_all();
    }

    /// Release any sample-entry hold and disarm it (c-ctxusage r22 item 3).
    #[cfg(feature = "test-faults")]
    pub(crate) fn release_sample_hold_state(&self) {
        self.sample_hold_armed.store(false, Ordering::SeqCst);
        let _ = self.sample_hold_tx.send(true);
    }

    /// Open the shared blocking-phase gate so a parked finalise/collect can
    /// finish (c-ctxusage r21 item 4): retirement that hits its bounded wait
    /// releases the parked phase instead of abandoning a parked OS thread.
    #[cfg(feature = "test-faults")]
    pub(crate) fn open_blocking_gate(&self) {
        if let Ok(mut open) = self.gate_open.lock() {
            *open = true;
        }
        self.gate_cv.notify_all();
    }

    /// Whether the blocking-phase gate has been opened (a parked finalise was
    /// released) rather than abandoned (c-ctxusage r21 item 4 test seam).
    #[cfg(feature = "test-faults")]
    #[must_use]
    pub(crate) fn blocking_gate_is_open_state(&self) -> bool {
        self.gate_open.lock().map(|open| *open).unwrap_or(false)
    }

    /// The injected bounded-pending capacity override, if a test armed one.
    pub(crate) fn pending_capacity(&self) -> Option<usize> {
        match self.pending_capacity.load(Ordering::SeqCst) {
            0 => None,
            n => Some(n as usize),
        }
    }

    /// The injected native stop-phase grace override (c-ctxusage r13 item 1).
    pub(crate) fn agent_exit_grace(&self) -> Option<Duration> {
        match self.agent_exit_grace_ms.load(Ordering::SeqCst) {
            0 => None,
            millis => Some(Duration::from_millis(millis)),
        }
    }

    /// Consume one injected close-time ownership-probe failure, if any remain
    /// (c-ctxusage r13 item 2). Models `resources.interrupt()`'s Herdr probe
    /// returning an error on THIS close attempt so the call returns before the
    /// native writer is stopped; a retried close probes cleanly.
    pub(crate) fn take_close_probe_failure(&self) -> bool {
        self.close_probe_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                if remaining > 0 {
                    Some(remaining - 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    /// Consume one armed shell-pty spawn failure (c-ctxusage r20 item 1).
    pub(crate) fn take_spawn_failure(&self) -> bool {
        self.spawn_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                if remaining > 0 {
                    Some(remaining - 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    /// Whether the flusher has already exited.
    #[cfg(feature = "test-faults")]
    fn flusher_exited_now(&self) -> bool {
        *self
            .flusher_exited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Test-only handle used to drive the injected faults. Constructible only
/// with the `test-faults` feature, which release binaries never enable.
#[cfg(feature = "test-faults")]
#[derive(Clone)]
pub struct TranscriptFaults {
    pub(crate) inner: Arc<FaultState>,
}

#[cfg(feature = "test-faults")]
impl TranscriptFaults {
    /// Create a fresh fault handle.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: FaultState::new(),
        }
    }

    /// Arm the gate that parks the first blocking transcript collect.
    pub fn arm_pump_gate(&self) {
        self.inner.pump_armed.store(true, Ordering::SeqCst);
    }

    /// Arm the gate that parks the cooperative finalise finish.
    pub fn arm_finish_gate(&self) {
        self.inner.finish_armed.store(true, Ordering::SeqCst);
    }

    /// Arm the gate that parks a blocking closure before it claims.
    pub fn arm_queued_gate(&self) {
        self.inner.queued_armed.store(true, Ordering::SeqCst);
    }

    /// Arm the ONE-SHOT gate that parks the first registered terminal
    /// finaliser before it claims the hydrator (c-ctxusage r10 item 1).
    pub fn arm_terminal_gate(&self) {
        self.inner.terminal_armed.store(true, Ordering::SeqCst);
    }

    /// Make the cooperative reading finalise a no-op.
    pub fn set_skip_cooperative_finalise(&self, value: bool) {
        self.inner
            .skip_cooperative_finalise
            .store(value, Ordering::SeqCst);
    }

    /// Shorten the driver's bounded cooperative-close wait so a parked pump
    /// crosses the abort bound quickly (production timeout is used otherwise).
    pub fn arm_close_timeout(&self, timeout: Duration) {
        self.inner.close_timeout_ms.store(
            timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            Ordering::SeqCst,
        );
    }

    /// Shrink the bounded producer→flusher queue so a normal-sized batch
    /// exceeds it (c-ctxusage r9 item 1).
    pub fn set_pending_capacity(&self, capacity: usize) {
        self.inner
            .pending_capacity
            .store(capacity as u64, Ordering::SeqCst);
    }

    /// Shorten the native stop-phase grace so a writer that does not finish on
    /// Ctrl-C reaches bounded forced termination quickly (c-ctxusage r13
    /// item 1; production grace is used otherwise).
    pub fn arm_agent_exit_grace(&self, grace: Duration) {
        self.inner.agent_exit_grace_ms.store(
            grace.as_millis().min(u128::from(u64::MAX)) as u64,
            Ordering::SeqCst,
        );
    }

    /// Make the next `attempts` close calls fail the native ownership probe
    /// once each — returning before the writer is stopped — modelling Herdr
    /// refusing `session.snapshot` in its shutdown window (c-ctxusage r13
    /// item 2). A later close retries the phase and probes cleanly.
    pub fn arm_close_probe_failures(&self, attempts: u32) {
        self.inner
            .close_probe_failures
            .store(u64::from(attempts), Ordering::SeqCst);
    }

    /// The next `attempts` shell-pty spawns fail after launch materialization
    /// (c-ctxusage r20 item 1), modelling a materialize/PTY-open/spawn_command
    /// error before the child process exists.
    pub fn arm_spawn_failures(&self, attempts: u32) {
        self.inner
            .spawn_failures
            .store(u64::from(attempts), Ordering::SeqCst);
    }

    /// Shorten the promoted-close poller handoff barrier so a test of a
    /// missed/foreign drain crosses the retryable bound quickly (P1,
    /// c-ctxusage r16 item 6). Production bound is used otherwise.
    pub fn arm_close_barrier_timeout(&self, timeout: Duration) {
        self.inner.close_barrier_timeout_ms.store(
            timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            Ordering::SeqCst,
        );
    }

    /// Make the next `attempts` promoted closes report the native writer as
    /// STILL LIVE after the stop ladder (P1, c-ctxusage r16 item 5): each such
    /// close returns the retryable `WriterStillLive` error without retiring
    /// the hydrator or dropping close ownership; the following close runs the
    /// real verification and completes.
    pub fn arm_promoted_stop_incomplete(&self, attempts: u32) {
        self.inner
            .promoted_stop_incomplete_attempts
            .store(u64::from(attempts), Ordering::SeqCst);
    }

    /// Arm the one-shot gate that parks the first close between its final
    /// drain and its reap/terminal emission (c-ctxusage r14 item 5).
    pub fn arm_close_tail_gate(&self) {
        self.inner.close_tail_armed.store(true, Ordering::SeqCst);
    }

    /// Release a close parked at the close-tail gate. Fires the installed
    /// one-shot; if the park has not installed it yet this is a no-op (the
    /// test waits for `wait_close_tail_entered` first, which is published
    /// only AFTER the sender is installed — c-ctxusage r15 item 9).
    pub fn release_close_tail(&self) {
        let sender = self
            .inner
            .close_tail_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
    }

    /// Block until the armed close reaches its tail park point (bounded).
    pub fn wait_close_tail_entered(&self) -> bool {
        Self::wait_true(
            &self.inner.close_tail_entered,
            &self.inner.close_tail_entered_cv,
            Duration::from_secs(60),
        )
    }

    /// Block until `n` close() calls have reached the close-phase entry
    /// (bounded, c-ctxusage r14 item 5).
    pub fn wait_close_phase_calls(&self, n: u64) -> bool {
        let guard = self
            .inner
            .close_phase_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, _) = self
            .inner
            .close_phase_calls_cv
            .wait_timeout_while(guard, Duration::from_secs(60), |calls| *calls < n)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard >= n
    }

    /// Open the gate and let every parked blocking phase proceed.
    pub fn release(&self) {
        *self
            .inner
            .gate_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.inner.gate_cv.notify_all();
    }

    /// Block until the pump-collect phase signals it entered (bounded).
    pub fn wait_pump_entered(&self) -> bool {
        Self::wait_true(
            &self.inner.pump_entered,
            &self.inner.pump_entered_cv,
            Duration::from_secs(60),
        )
    }

    /// Block until the cooperative-finish phase signals it entered (bounded).
    pub fn wait_finish_entered(&self) -> bool {
        Self::wait_true(
            &self.inner.finish_entered,
            &self.inner.finish_entered_cv,
            Duration::from_secs(60),
        )
    }

    /// Block until the queued blocking closure signals it entered (bounded).
    pub fn wait_queued_entered(&self) -> bool {
        Self::wait_true(
            &self.inner.queued_entered,
            &self.inner.queued_entered_cv,
            Duration::from_secs(60),
        )
    }

    /// Block until the first registered terminal finaliser signals it parked
    /// before its claim (bounded, c-ctxusage r10 item 1).
    pub fn wait_terminal_entered(&self) -> bool {
        Self::wait_true(
            &self.inner.terminal_entered,
            &self.inner.terminal_entered_cv,
            Duration::from_secs(60),
        )
    }

    /// Block until the abort guard's residual-finish thread acknowledges it
    /// ran (bounded).
    pub fn wait_abort_residual(&self) -> bool {
        Self::wait_true(
            &self.inner.abort_residual,
            &self.inner.abort_residual_cv,
            Duration::from_secs(60),
        )
    }

    /// Block until the abort residual's second terminal registers and parks
    /// behind the elected owner (bounded, c-ctxusage r11 item 3).
    pub fn wait_abort_terminal_waiting_for(&self, timeout: Duration) -> bool {
        Self::wait_true(
            &self.inner.abort_terminal_waiting,
            &self.inner.abort_terminal_waiting_cv,
            timeout,
        )
    }

    /// Non-blocking check whether the streaming flusher has exited.
    #[must_use]
    pub fn flusher_exited(&self) -> bool {
        self.inner.flusher_exited_now()
    }

    /// Wait up to `timeout` for the streaming flusher to exit; returns
    /// whether it exited within the window.
    pub fn wait_flusher_exit_for(&self, timeout: Duration) -> bool {
        Self::wait_true(
            &self.inner.flusher_exited,
            &self.inner.flusher_exited_cv,
            timeout,
        )
    }

    /// Block until a promoted close parks at the poller handoff barrier
    /// (P1, c-ctxusage r16 item 6), bounded.
    pub fn wait_close_barrier_waiting_for(&self, timeout: Duration) -> bool {
        Self::wait_true(
            &self.inner.close_barrier_waiting,
            &self.inner.close_barrier_waiting_cv,
            timeout,
        )
    }

    /// Whether `n` promoted closes armed the handoff barrier within
    /// `timeout` (c-ctxusage r17 item 3): a concurrent close serialized
    /// behind the first must not arm until the first handoff resolves.
    pub fn wait_close_barrier_arms(&self, n: u64, timeout: Duration) -> bool {
        let guard = self
            .inner
            .close_barrier_arms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, _) = self
            .inner
            .close_barrier_arms_cv
            .wait_timeout_while(guard, timeout, |arms| *arms < n)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard >= n
    }

    /// Number of promotion poller tasks spawned so far (c-ctxusage r20
    /// item 2): a non-blocking hard assertion at a parked barrier.
    #[must_use]
    pub fn promoter_start_count(&self) -> u64 {
        *self
            .inner
            .promoter_starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Block until `n` promotion pollers have spawned, bounded
    /// (c-ctxusage r20 item 2).
    pub fn wait_promoter_starts(&self, n: u64, timeout: Duration) -> bool {
        self.inner.wait_promoter_starts_state(n, timeout)
    }

    /// Hold NORMAL promotion samples (c-ctxusage r21 item 3). Close, live
    /// drain and shutdown wakeups still pass while armed; the poll loop parks
    /// at an async gate until [`Self::release_normal_samples`].
    pub fn arm_normal_sample_gate(&self) {
        // Re-arm the watch if a previous run already released it.
        if *self.inner.normal_gate_tx.borrow() {
            let _ = self.inner.normal_gate_tx.send(false);
        }
        self.inner.normal_gate_armed.store(true, Ordering::SeqCst);
    }

    /// Resume normal sampling (c-ctxusage r21 item 3).
    pub fn release_normal_samples(&self) {
        self.inner.release_normal_samples_state();
    }

    /// Block until `n` normal samples parked, bounded (c-ctxusage r21 item 3).
    pub fn wait_normal_samples_parked(&self, n: u64, timeout: Duration) -> bool {
        let guard = self
            .inner
            .normal_parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, _) = self
            .inner
            .normal_parked_cv
            .wait_timeout_while(guard, timeout, |parked| *parked < n)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard >= n
    }

    /// Whether the normal-sample gate is armed (c-ctxusage r21 item 3).
    #[must_use]
    pub fn normal_sample_gate_armed(&self) -> bool {
        self.inner.normal_gate_is_armed()
    }

    /// Count of normal samples that have parked (c-ctxusage r21 item 3).
    #[must_use]
    pub fn normal_sample_park_count(&self) -> u64 {
        *self
            .inner
            .normal_parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hold a normal sample at the very start of sampling (c-ctxusage r22
    /// item 3): a probe that the normal-sample gate stops sampling itself.
    pub fn arm_sample_hold(&self) {
        if *self.inner.sample_hold_tx.borrow() {
            let _ = self.inner.sample_hold_tx.send(false);
        }
        *self
            .inner
            .sample_hold_entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        self.inner.sample_hold_armed.store(true, Ordering::SeqCst);
    }

    /// Release the sample-entry hold (c-ctxusage r22 item 3).
    pub fn release_sample_hold(&self) {
        self.inner.release_sample_hold_state();
    }

    /// Block until a normal sample reaches the entry hold, bounded
    /// (c-ctxusage r22 item 3).
    pub fn wait_sample_hold_entered(&self, timeout: Duration) -> bool {
        Self::wait_true(
            &self.inner.sample_hold_entered,
            &self.inner.sample_hold_entered_cv,
            timeout,
        )
    }

    /// Whether a normal sample entered the entry hold (c-ctxusage r22 item 3).
    #[must_use]
    pub fn sample_hold_entered(&self) -> bool {
        *self
            .inner
            .sample_hold_entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether retirement opened (released) a parked blocking phase rather
    /// than abandoning it (c-ctxusage r21 item 4).
    #[must_use]
    pub fn blocking_gate_is_open(&self) -> bool {
        self.inner.blocking_gate_is_open_state()
    }

    /// Block until at least `n` promotion polls have bound a transcript
    /// (across runs), bounded (c-ctxusage r19 item 1). Used to prove the
    /// resumed run's poller bound WHILE the predecessor finalise was parked.
    pub fn wait_promoter_binds(&self, n: u64, timeout: Duration) -> bool {
        let guard = self
            .inner
            .promoter_binds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, _) = self
            .inner
            .promoter_binds_cv
            .wait_timeout_while(guard, timeout, |binds| *binds < n)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard >= n
    }

    /// Block until the streaming flusher acknowledges it drained and parked
    /// idle (bounded).
    pub fn wait_flusher_idle(&self) -> bool {
        Self::wait_true(
            &self.inner.flusher_idle,
            &self.inner.flusher_idle_cv,
            Duration::from_secs(60),
        )
    }

    /// Block until the promoter task acknowledges it spawned with the
    /// flusher/gate handshake live (bounded, c-ctxusage r10 item 5).
    pub fn wait_promoter_started_for(&self, timeout: Duration) -> bool {
        Self::wait_true(
            &self.inner.promoter_started,
            &self.inner.promoter_started_cv,
            timeout,
        )
    }

    fn wait_true(flag: &Mutex<bool>, cv: &Condvar, timeout: Duration) -> bool {
        let mut guard = flag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = std::time::Instant::now() + timeout;
        while !*guard {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (g, timeout) = cv
                .wait_timeout(guard, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = g;
            if timeout.timed_out() {
                break;
            }
        }
        *guard
    }
}

#[cfg(feature = "test-faults")]
impl Default for TranscriptFaults {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, feature = "test-faults"))]
mod terminal_gate_tests {
    use super::*;
    use std::sync::PoisonError;

    const WAIT: Duration = Duration::from_secs(5);

    fn join_bounded(handle: std::thread::JoinHandle<()>, what: &str) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _watcher = std::thread::spawn(move || {
            let _ = tx.send(handle.join());
        });
        match rx.recv_timeout(WAIT) {
            Ok(Ok(())) => {}
            Ok(Err(payload)) => std::panic::resume_unwind(payload),
            Err(err) => panic!("{what} within {WAIT:?}: {err:?}"),
        }
    }

    /// c-ctxusage r12 item 3: the abort residual's terminal can execute BEFORE
    /// the merely-queued cooperative closure reaches the seam. The old
    /// first-executor swap made whichever terminal ran first consume the
    /// cooperative pre-claim gate, so the test could release without the
    /// cooperative terminal ever registering. Only the cooperative terminal
    /// (close owner, registration ticket 0) may park.
    #[test]
    fn the_abort_terminal_executing_first_never_consumes_the_cooperative_gate() {
        let faults = FaultState::new();
        faults.terminal_armed.store(true, Ordering::SeqCst);

        // Abort residual: registered second (ticket 1), executes first.
        let abort_faults = Arc::clone(&faults);
        let abort = std::thread::spawn(move || abort_faults.terminal_finalise_queued(1, false));
        join_bounded(abort, "the abort terminal parked in the cooperative gate");
        assert!(
            !*faults
                .terminal_entered
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            "the abort residual acknowledged the cooperative gate"
        );

        // Cooperative close terminal (ticket 0) parks and acknowledges.
        let coop_faults = Arc::clone(&faults);
        let coop = std::thread::spawn(move || coop_faults.terminal_finalise_queued(0, true));
        {
            let guard = faults
                .terminal_entered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (guard, wait) = faults
                .terminal_entered_cv
                .wait_timeout_while(guard, WAIT, |entered| !*entered)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                !wait.timed_out(),
                "the cooperative terminal never acknowledged its pre-claim park"
            );
            assert!(
                *guard,
                "the cooperative terminal never acknowledged its pre-claim park"
            );
        }

        // A boundary terminal (non-cooperative) never parks either.
        let boundary_faults = Arc::clone(&faults);
        let boundary =
            std::thread::spawn(move || boundary_faults.terminal_finalise_queued(2, false));
        join_bounded(boundary, "a boundary terminal parked in the close gate");

        *faults
            .gate_open
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        faults.gate_cv.notify_all();
        join_bounded(coop, "the cooperative terminal never left its park");
    }

    /// c-ctxusage r13 item 7: an in-life demote/rebind BEFORE close runs a
    /// boundary terminal finish that registers FIRST and consumes ticket 0
    /// (the producer gate survives the boundary and keeps incrementing
    /// tickets). The close-time cooperative terminal therefore holds a LATER
    /// ticket. The gate must select it by its cooperative flag - parking it
    /// and capturing its real ticket - instead of requiring hardcoded ticket
    /// zero, which would let the close terminal sail straight through. The
    /// abort residual stays out regardless of its ticket.
    #[test]
    fn a_cooperative_terminal_after_a_demote_rebind_parks_on_its_real_ticket() {
        let faults = FaultState::new();
        faults.terminal_armed.store(true, Ordering::SeqCst);

        // In-life demote/rebind boundary finish: ticket 0, non-cooperative.
        // It returns immediately and never touches the cooperative gate.
        let boundary_faults = Arc::clone(&faults);
        let boundary =
            std::thread::spawn(move || boundary_faults.terminal_finalise_queued(0, false));
        join_bounded(boundary, "the in-life boundary terminal parked");
        assert!(
            !*faults
                .terminal_entered
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            "an in-life boundary finish must not acknowledge the close gate"
        );
        assert!(
            faults
                .cooperative_ticket
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "no cooperative ticket may be captured for a boundary finish"
        );

        // The close cooperative terminal now registers on ticket 1 and parks,
        // even though it is not ticket zero.
        let coop_faults = Arc::clone(&faults);
        let coop = std::thread::spawn(move || coop_faults.terminal_finalise_queued(1, true));
        {
            let guard = faults
                .cooperative_ticket
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let (guard, wait) = faults
                .cooperative_ticket_cv
                .wait_timeout_while(guard, WAIT, |ticket| ticket.is_none())
                .unwrap_or_else(PoisonError::into_inner);
            assert!(
                !wait.timed_out(),
                "the post-rebind cooperative terminal never parked (ticket 1)"
            );
            assert_eq!(
                *guard,
                Some(1),
                "the gate must capture the cooperative terminal's real later ticket"
            );
        }
        assert!(
            *faults
                .terminal_entered
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            "the post-rebind cooperative terminal must acknowledge its park"
        );

        // The abort residual after the rebind is ticket 2 and non-cooperative:
        // it returns immediately and cannot steal or overwrite the park.
        let residual_faults = Arc::clone(&faults);
        let residual =
            std::thread::spawn(move || residual_faults.terminal_finalise_queued(2, false));
        join_bounded(residual, "the abort residual parked in the close gate");
        assert_eq!(
            *faults
                .cooperative_ticket
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Some(1),
            "the residual must not overwrite the captured cooperative ticket"
        );

        *faults
            .gate_open
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        faults.gate_cv.notify_all();
        join_bounded(
            coop,
            "the post-rebind cooperative terminal never left its park",
        );
    }
}

#[cfg(all(test, feature = "test-faults"))]
mod close_tail_gate_tests {
    use super::*;
    use std::sync::Arc;

    /// c-ctxusage r15 item 9: the release must not be lost when it lands at
    /// the exact protocol point the test is synchronised on — immediately
    /// after the entered acknowledgement, before the parked close task has
    /// necessarily registered its wait. The old `Notify::notify_waiters`
    /// shape dropped a wake with no registered waiter, so the close sat in
    /// its park until the 120 s bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_races_park_registration_and_still_opens_the_gate() {
        let faults = TranscriptFaults::new();
        let state = Arc::clone(&faults.inner);
        faults.arm_close_tail_gate();
        let parked = tokio::spawn(async move {
            state.await_close_tail_gate().await;
        });
        // The documented protocol point: only release once the entered ack
        // is observable (published AFTER the release sender is installed).
        assert!(
            faults.wait_close_tail_entered(),
            "the parked close never published its entered ack"
        );
        faults.release_close_tail();
        // With the oneshot the wake is buffered: the parked task completes
        // promptly regardless of how its scheduling lands. Five seconds is
        // far inside the gate's 120 s hang bound.
        tokio::time::timeout(std::time::Duration::from_secs(5), parked)
            .await
            .expect("the parked close never observed its release")
            .expect("the parked close task panicked");
    }
}
