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
    skip_cooperative_finalise: AtomicBool,
    /// Overrides the driver's bounded cooperative-close wait so an abort test
    /// crosses the bound deterministically without the production 15 s / 2 s
    /// wait. Milliseconds; 0 means "use the production timeout".
    close_timeout_ms: AtomicU64,
    /// Overrides the bounded pending queue capacity so a test can make a
    /// normal-sized replay batch exceed it. 0 means production capacity.
    pending_capacity: AtomicU64,
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
            skip_cooperative_finalise: AtomicBool::new(false),
            close_timeout_ms: AtomicU64::new(0),
            pending_capacity: AtomicU64::new(0),
        })
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
    /// The gate admits EXACTLY the cooperative terminal: `cooperative` marks
    /// the cooperative close finalise (never the abort residual and never an
    /// in-life boundary finish), and ticket 0 is the first terminal
    /// registered — the cooperative finalise always registers before close's
    /// timeout can abort and register the residual. Selecting on BOTH (instead
    /// of a first-executor swap, c-ctxusage r12 item 3) means the abort thread
    /// can never steal the cooperative pre-claim barrier when it executes
    /// first; it reaches the gate claim and parks there instead.
    pub(crate) fn terminal_finalise_queued(
        &self,
        ticket: crate::producer_gate::TerminalTicket,
        cooperative: bool,
    ) {
        if !self.terminal_armed.load(Ordering::SeqCst) || !cooperative || ticket != 0 {
            return;
        }
        *self
            .terminal_entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.terminal_entered_cv.notify_all();
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
    }

    /// The injected bounded-pending capacity override, if a test armed one.
    pub(crate) fn pending_capacity(&self) -> Option<usize> {
        match self.pending_capacity.load(Ordering::SeqCst) {
            0 => None,
            n => Some(n as usize),
        }
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
}
