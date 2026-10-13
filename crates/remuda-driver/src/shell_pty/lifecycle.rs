//! Process-group stop ladder and exit detection for [`super::ShellPtyDriver`]
//! (D-028 §5.3, §5.5).
//!
//! Two facts this module exists to keep apart:
//!
//! * **Interrupting a turn** ends one round of work; the session and the
//!   process both stay alive. That is a *keystroke*, harness-specific, and
//!   lives in [`super::keys`].
//! * **Stopping the process** terminates everything this instance launched.
//!   That is a *signal ladder*, and it is here.
//!
//! ## Why the process group and not the child
//!
//! `portable-pty` calls `setsid()` in the forked child before `exec`, so the
//! PTY's child is a session leader and its own process-group leader. Everything
//! it starts — a login shell, the `claude` the human typed into it, the `node`
//! that `claude` really is, and any tool subprocess — inherits that group
//! unless it deliberately leaves. `kill(child_pid)` reaches exactly one of
//! those; `killpg(pgid)` reaches all of them. The pre-D-028 code sent
//! `child.kill()`, which is a `SIGKILL` to the login shell alone and orphans
//! the agent (§5.3), and it sent even that only when `try_lock` happened to
//! succeed — a lock the reader thread holds for the whole of a `wait()`, so the
//! common case was *no signal at all*.

use crate::error::{DriverError, DriverResult};
use std::time::Duration;

/// Grace after `SIGINT` before escalating. §5.3 step 1.
pub(super) const GRACE_INT: Duration = Duration::from_secs(2);
/// Grace after `SIGHUP` before `SIGKILL`. §5.3 step 2.
pub(super) const GRACE_HUP: Duration = Duration::from_secs(2);
/// Grace after `SIGKILL` before declaring the group un-killable.
///
/// `SIGKILL` cannot be caught, so anything still here is blocked in the kernel
/// (uninterruptible IO, a zombie nobody reaped). Short, because the only honest
/// outcome after it expires is the `stop-incomplete` diagnostic.
pub(super) const GRACE_KILL: Duration = Duration::from_millis(500);

/// How often the ladder rechecks whether the group has gone.
const POLL: Duration = Duration::from_millis(25);

/// Reap poll used by the driver after the ladder reports the group gone, while
/// waiting for an exiting leader (macOS `E`) to become reapable.
pub(super) const EXIT_REAP_POLL: Duration = POLL;

/// Which rung actually ended the process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopRung {
    /// Nothing was running by the time the ladder started.
    AlreadyGone,
    /// `SIGINT` was enough.
    Interrupt,
    /// `SIGHUP` (plus closing the master) was needed.
    Hangup,
    /// `SIGKILL` was needed.
    Kill,
}

impl StopRung {
    /// Wire name for the journal's `related_ids`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::AlreadyGone => "already-gone",
            Self::Interrupt => "sigint",
            Self::Hangup => "sighup",
            Self::Kill => "sigkill",
        }
    }
}

/// What the ladder concluded. §5.3 step 4 forbids reporting `exited` when the
/// group is still there, so "did it actually go" is a separate field from "how
/// far did we escalate".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopOutcome {
    /// Highest rung reached.
    pub rung: StopRung,
    /// The group is confirmed gone. `false` means `stop-incomplete`.
    pub group_gone: bool,
    /// Process group that was signalled, when there was one.
    pub pgid: Option<i32>,
    /// Survivor pids on a `stop-incomplete`, for the diagnostic.
    pub survivors: Vec<i32>,
    /// Ruling F' (c-usagefu r16 item 2): the pre-signal process-tree SNAPSHOT
    /// itself could not be read, so no descendant identity was ever captured.
    /// `true` forces `group_gone = false`; the pending-termination entry can
    /// never be proven gone by this Node (Unknown never closes) and is
    /// reconciled by the next Node process. A failed post-ladder RESCAN with a
    /// captured tree does NOT set this — its members are retained as
    /// Unknown-strength identities and later confirmed probes may close them.
    pub observation_unknown: bool,
}

impl StopOutcome {
    /// Nothing to stop.
    #[must_use]
    pub fn already_gone() -> Self {
        Self {
            rung: StopRung::AlreadyGone,
            group_gone: true,
            pgid: None,
            survivors: Vec::new(),
            observation_unknown: false,
        }
    }
}

/// Send `signal` to process group `pgid`.
///
/// `ESRCH` (no such group) is success: the point of the call is that the group
/// is gone, and it already is.
#[cfg(unix)]
fn signal_group(pgid: i32, signal: nix::sys::signal::Signal) -> DriverResult<()> {
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(errno) => Err(DriverError::Io(std::io::Error::from_raw_os_error(
            errno as i32,
        ))),
    }
}

#[cfg(not(unix))]
fn signal_group(pgid: i32, _signal: ()) -> DriverResult<()> {
    let _ = pgid;
    Err(DriverError::CapabilityUnsupported(
        "process-group signalling requires unix".into(),
    ))
}

/// Whether process group `pgid` still has any member.
///
/// `kill(-pgid, 0)` asks the kernel rather than parsing `ps`, so a process the
/// caller may not signal (`EPERM`) still counts as present — refusing to see a
/// process we cannot kill would be exactly the false `exited` §5.3 forbids.
#[must_use]
pub fn group_alive(pgid: i32) -> bool {
    #[cfg(unix)]
    {
        if pgid <= 0 {
            return false;
        }
        match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None) {
            Ok(()) => true,
            Err(nix::errno::Errno::EPERM) => true,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

/// What the host can establish about one child process BEFORE its parent
/// reaps it (ma-sdk-state r14 item 4; r15 item 2).
///
/// The distinction between "missing" and "unknown" is load-bearing: an
/// unreadable `/proc` on Linux must never be read as death (the r13 test
/// helper mapped every read error to "gone", which is always true on macOS
/// where `/proc` does not exist), and signal 0 alone must never be read as
/// liveness on macOS — it succeeds for an unreaped zombie, which the pre-close
/// SIGKILL proofs must be able to see (r15 item 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildDisposition {
    /// No such process: it exited and has already been reaped (or the pid is
    /// stale). Signal 0 returns `ESRCH` (Linux also: `/proc/<pid>` is ENOENT).
    Gone,
    /// Dead but not yet reaped by its parent: Linux `/proc` state `Z`/`X`/`x`;
    /// a non-Linux unix `ps` row in state `Z`/`E`/`?Es`.
    Zombie,
    /// Still running.
    Alive,
    /// The information is unavailable (unreadable `/proc` for a reason other
    /// than ENOENT, `EPERM` from signal 0, or a `ps` table that does not show
    /// the pid). Callers that need proof of death keep waiting rather than
    /// treating this as gone.
    Unknown,
}

/// Classify one direct child from its Linux `/proc/<pid>/stat` contents and a
/// signal-0 probe result.
///
/// EVERY `/proc` read failure — INCLUDING `NotFound` — falls through to the
/// signal-0 probe. ENOENT normally means the pid was reaped, but inside a
/// chroot or mount namespace `/proc/<pid>` is absent while the child is very
/// much alive, so the read result alone is never death:
/// [`ChildDisposition::Gone`] requires a confirming `ESRCH`, while existence
/// (`Ok`) or `EPERM` under an unreadable `/proc` is
/// [`ChildDisposition::Unknown`] (r15 item 3).
#[cfg(target_os = "linux")]
pub(crate) fn classify_linux_child(
    stat: std::io::Result<String>,
    signal0: Result<(), nix::errno::Errno>,
) -> ChildDisposition {
    match stat {
        Ok(text) => classify_linux_stat(&text),
        Err(_) => match signal0 {
            // The process exists (or /proc itself is hidden from this
            // namespace) but /proc would not say in what state: unavailable
            // information, not evidence of death.
            Ok(()) | Err(nix::errno::Errno::EPERM) => ChildDisposition::Unknown,
            // ENOENT plus a confirming ESRCH is the normal reaped/gone case.
            Err(nix::errno::Errno::ESRCH) => ChildDisposition::Gone,
            Err(_) => ChildDisposition::Unknown,
        },
    }
}

/// Parse the state letter out of a `/proc/<pid>/stat` line ("<comm>) <state> …";
/// comm can contain spaces/parens, so split at the LAST ')'. An unparseable
/// line is [`ChildDisposition::Unknown`] rather than a guess.
#[cfg(target_os = "linux")]
fn classify_linux_stat(stat: &str) -> ChildDisposition {
    match stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
    {
        Some("Z") | Some("X") | Some("x") => ChildDisposition::Zombie,
        Some(_) => ChildDisposition::Alive,
        None => ChildDisposition::Unknown,
    }
}

/// Classify a child on a unix WITHOUT `/proc` (macOS, *BSD) from a signal-0
/// probe result and the child's `ps` state row, if `ps` showed one
/// (ma-sdk-state r15 item 2).
///
/// Signal 0 alone is NOT liveness evidence on these platforms: `kill(pid, 0)`
/// succeeds for an unreaped zombie exactly as for a running process, which is
/// what made the old arm answer `Alive` for a SIGKILLed child and drove the
/// pre-close death proofs into their full timeout and panic. The rules are:
///
/// * signal 0 `ESRCH` (whatever `ps` saw) → [`ChildDisposition::Gone`];
/// * signal 0 `Ok` plus a `ps` row with a dead-state letter (`Z`, `E`,
///   `?Es`, … — [`state_is_dead`]) → [`ChildDisposition::Zombie`];
/// * signal 0 `Ok` plus a live row → [`ChildDisposition::Alive`];
/// * signal 0 `Ok` with NO row (the `ps` invocation failed or the table does
///   not show the pid) → [`ChildDisposition::Unknown`]: existence without a
///   state is unavailable information, never liveness;
/// * signal 0 `EPERM`/any other error → [`ChildDisposition::Unknown`].
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
pub(crate) fn classify_ps_child(
    signal0: Result<(), nix::errno::Errno>,
    ps_state: Option<&str>,
) -> ChildDisposition {
    match signal0 {
        Err(nix::errno::Errno::ESRCH) => ChildDisposition::Gone,
        Ok(()) => match ps_state {
            Some(state) if state_is_dead(state) => ChildDisposition::Zombie,
            Some(_) => ChildDisposition::Alive,
            None => ChildDisposition::Unknown,
        },
        // EPERM: the pid exists but we may not signal it — ps rows for it
        // cannot be trusted to establish liveness under the same restriction,
        // and every other signal error is equally inconclusive.
        Err(_) => ChildDisposition::Unknown,
    }
}

/// Read one pid's state letter from `ps -o pid=,stat= -p <pid>` output.
/// Split out from the invocation so the macOS classification has a
/// host-independent unit test with injected output.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
pub(crate) fn parse_ps_pid_state(bytes: &[u8], wanted: i32) -> Option<String> {
    for line in String::from_utf8_lossy(bytes).lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        let state = parts.next().unwrap_or("");
        if pid == wanted {
            return Some(state.to_owned());
        }
    }
    None
}

/// Establish one child's disposition (ma-sdk-state r14 item 4; r15 items 2/3).
///
/// * **Linux**: `/proc/<pid>/stat` distinguishes a live child from an
///   unreaped zombie — exactly the state a SIGKILLed child sits in before its
///   parent's `wait()`. Any read failure, INCLUDING ENOENT (a chroot or mount
///   namespace can hide all of `/proc` while the child lives), is confirmed
///   against signal 0: gone needs a matching `ESRCH`.
/// * **macOS/other unix**: there is no `/proc`; signal 0 asks the kernel
///   whether the pid exists and `ps` supplies the state signal 0 cannot:
///   `ESRCH` is gone/reaped, a dead-state row (`Z`/`E`/`?Es`) is an unreaped
///   zombie, a live row is alive, and anything less conclusive is unknown.
///   Callers requiring gone/zombie evidence keep waiting.
#[must_use]
pub fn child_disposition(pid: i32) -> ChildDisposition {
    if pid <= 0 {
        return ChildDisposition::Unknown;
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        let signal0 = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None);
        classify_linux_child(stat, signal0)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let signal0 = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None);
        // `ps` can add nothing on a confirming ESRCH; for every other answer
        // the state row is what separates a zombie from a live process.
        let ps_state = if matches!(signal0, Err(nix::errno::Errno::ESRCH)) {
            None
        } else {
            ps_child_state(pid)
        };
        classify_ps_child(signal0, ps_state.as_deref())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        ChildDisposition::Unknown
    }
}

/// Invoke `ps` for one pid's state row on a non-Linux unix (ma-sdk-state
/// r15 item 2). Returns `None` when `ps` cannot be run, fails, or shows no
/// row — all indistinguishable cases the classifier reads as unknown.
#[cfg(all(unix, not(target_os = "linux")))]
fn ps_child_state(pid: i32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "pid=,stat=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_ps_pid_state(&output.stdout, pid)
}

/// Whether a single process (by pid, not group) is still running.
///
/// Like [`group_alive`], this asks the kernel with signal 0 rather than parsing
/// the process table, and counts an `EPERM` process as present. Use this for a
/// child whose pid is *not* its group leader (a grandchild): `kill(pid, 0)`
/// targets one process, whereas `killpg(pid, 0)` would look for a group whose id
/// is `pid`, a different thing.
#[must_use]
pub fn process_alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        if pid <= 0 {
            return false;
        }
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
            Ok(()) => true,
            Err(nix::errno::Errno::EPERM) => true,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Wait (event-driven, no poll/sleep) for a process the caller did NOT fork
/// to exit, then confirm the disposition reads [`ChildDisposition::Gone`] or
/// [`ChildDisposition::Zombie`] (ma-sdk-state r15 item 2).
///
/// The pre-close SIGKILL proofs watch the DRIVER's direct child, so
/// `waitpid(2)` returns `ECHILD` and a `/proc`-state poll with a sleep was the
/// only option; the kernel instead offers a termination event:
///
/// * **Linux**: `pidfd_open(2)` + `poll(2)` (`POLLIN` on exit, kernel ≥5.3).
///   The fd becomes readable at the zombie transition for a non-child too.
/// * **macOS/BSD**: a `kqueue` `EVFILT_PROC`/`NOTE_EXIT` registration.
///
/// Returns `false` on timeout, on an unsupported host (`pidfd_open` absent),
/// or when the final classifier read is still inconclusive — the caller's
/// named bound is the terminator either way. Blocking work runs off the async
/// worker threads.
#[cfg(any(test, feature = "test-stub"))]
pub async fn wait_child_exit(pid: i32, timeout: Duration) -> bool {
    if pid <= 0 {
        return false;
    }
    let join = tokio::task::spawn_blocking(move || wait_child_exit_blocking(pid, timeout, None));
    join.await.unwrap_or_default()
}

/// What one bounded event wait ended on (ma-sdk-state r16 item 2). The
/// `Fired` payload carries the parsed event for the platform waiter (the
/// Linux arm carries nothing; the macOS arm carries its [`KqueueEventRead`]).
#[cfg(all(any(test, feature = "test-stub"), unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptibleEventWait<T> {
    /// The termination event fired.
    Fired(T),
    /// The single bound elapsed without an event.
    TimedOut,
}

/// Test-only probe for the REAL interruptible event wait (ma-sdk-state r17
/// item 2; r18 item 3): the ONE instant from which the waiter derives its
/// deadline (sampled where the deadline is computed and stamped into the
/// probe unchanged — the test clocks `completion - started` against the
/// bound) and the number of EINTR restarts the wait ACTUALLY performed (a
/// signal that was merely caught without interrupting the parked syscall
/// must not count).
#[cfg(all(any(test, feature = "test-stub"), unix))]
#[derive(Default)]
struct EintrWaitProbe {
    /// The single `started` instant the deadline is `started + timeout`
    /// from; stamped with exactly that instant (never a second sample).
    deadline_anchor: std::sync::Mutex<Option<std::time::Instant>>,
    /// Incremented on every EINTR restart against that same deadline.
    eintr_retries: std::sync::atomic::AtomicUsize,
}

#[cfg(all(any(test, feature = "test-stub"), unix))]
impl EintrWaitProbe {
    fn note_wait_started(&self, started: std::time::Instant) {
        *self.deadline_anchor.lock().expect("eintr probe anchor") = Some(started);
    }

    fn note_eintr_retry(&self) {
        self.eintr_retries
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    // Read accessors: deadline_anchor is read by both the all-unix r19 item 2
    // scripted-clock anchor-identity test and the Linux x86_64 real-syscall
    // test, so it compiles on every unix test build; eintr_retries is read
    // ONLY by the Linux x86_64 real-syscall test and must carry that gate or
    // macOS clippy reports it never used (ma-sdk-state r20).
    #[cfg(all(test, unix))]
    fn deadline_anchor(&self) -> Option<std::time::Instant> {
        *self.deadline_anchor.lock().expect("eintr probe anchor")
    }

    #[cfg(all(test, unix, target_os = "linux", target_arch = "x86_64"))]
    fn eintr_retries(&self) -> usize {
        self.eintr_retries
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Drive an interruptible bounded event wait against ONE monotonic deadline
/// (ma-sdk-state r16 item 2).
///
/// `wait_once` performs ONE blocking wait and receives the time REMAINING at the
/// deadline on every call — never the original timeout. `EINTR` restarts the
/// wait with that remainder (so a signal interrupting a parked `poll(2)` /
/// `kevent(2)` fails the waiter neither early nor after a freshly renewed full
/// timeout); any other error propagates immediately, and a deadline already
/// consumed ends the wait as [`InterruptibleEventWait::TimedOut`]. The event
/// source is level-triggered on Linux (a pidfd stays readable), so restarting
/// the wait loses no event.
#[cfg(all(any(test, feature = "test-stub"), unix))]
fn wait_with_eintr_retry<T>(
    timeout: Duration,
    probe: Option<&EintrWaitProbe>,
    wait_once: impl FnMut(Duration) -> std::io::Result<InterruptibleEventWait<T>>,
) -> std::io::Result<InterruptibleEventWait<T>> {
    wait_with_eintr_retry_clocked(timeout, probe, &mut std::time::Instant::now, wait_once)
}

/// Clock-seamed core of [`wait_with_eintr_retry`] (ma-sdk-state r19 item 2):
/// the real waiter passes `Instant::now`; the deterministic anchor-identity
/// regression passes a scripted clock so it can prove — without sleeps or
/// timing tolerance — that the probe stamps the SAME instant the deadline is
/// derived from.
#[cfg(all(any(test, feature = "test-stub"), unix))]
fn wait_with_eintr_retry_clocked<T>(
    timeout: Duration,
    probe: Option<&EintrWaitProbe>,
    now: &mut impl FnMut() -> std::time::Instant,
    mut wait_once: impl FnMut(Duration) -> std::io::Result<InterruptibleEventWait<T>>,
) -> std::io::Result<InterruptibleEventWait<T>> {
    // ma-sdk-state r18 item 3: sample ONE started instant; the deadline is
    // derived from it and the probe stamps EXACTLY that instant, so the
    // test's `completion - started` is measured against the same clock the
    // retry loop uses (previously the deadline used one Instant::now() sample
    // and the probe stamped a later one).
    let started = now();
    let deadline = started + timeout;
    if let Some(probe) = probe {
        probe.note_wait_started(started);
    }
    loop {
        let remaining = deadline.saturating_duration_since(now());
        if remaining == Duration::ZERO {
            return Ok(InterruptibleEventWait::TimedOut);
        }
        match wait_once(remaining) {
            Ok(outcome) => return Ok(outcome),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                // r17 item 2: acknowledge the ACTUAL restart of the real wait
                // (the injected unit tests pass no probe).
                if let Some(probe) = probe {
                    probe.note_eintr_retry();
                }
                continue;
            }
            Err(error) => return Err(error),
        }
    }
}

/// What one `kevent(2)` wait on the macOS/BSD arm ended on (ma-sdk-state
/// r16 item 4), expressed without nix types so the decision below is
/// unit-testable on EVERY unix target — the kqueue arm itself builds only off
/// Linux and the coordinator runs it on a Mac.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KqueueWaitOutcome {
    /// The eventlist entry fired with NOTE_EXIT (no EV_ERROR).
    NoteExit,
    /// The returned entry carried EV_ERROR: the changelist registration
    /// FAILED — this is NOT an exit (its payload is the errno, e.g. ESRCH for
    /// a process that died in the registration window, EPERM when the pid is
    /// not watchable). The old arm read every `Ok(1)` as an exit.
    ChangeError(i32),
    /// `kevent` returned 0: the bound elapsed with no event.
    BoundElapsed,
    /// The `kevent` syscall itself failed with an error other than EINTR
    /// (EINTR never reaches here — the retry helper owns it).
    WaitFailed,
}

/// The pre-arm move of the macOS child-exit wait (ma-sdk-state r16 item 4).
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KqueuePreArm {
    /// The disposition already reads Gone or Zombie: no kqueue is needed.
    /// This is the defect fix: EVFILT_PROC/NOTE_EXIT is edge-triggered and
    /// never replays a death that happened BEFORE registration, so an
    /// already-exited unreaped zombie used to wait out the full bound.
    AlreadyConfirmed,
    /// Register NOTE_EXIT and wait: the child is Alive, or the disposition is
    /// Unknown (a `ps` hiccup, EPERM), in which case kqueue is the last
    /// available event source.
    RegisterAndWait,
}

/// Decide the pre-arm move from an injected/real disposition (r16 item 4).
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
fn kqueue_pre_arm_move(disposition: ChildDisposition) -> KqueuePreArm {
    match disposition {
        ChildDisposition::Gone | ChildDisposition::Zombie => KqueuePreArm::AlreadyConfirmed,
        ChildDisposition::Alive | ChildDisposition::Unknown => KqueuePreArm::RegisterAndWait,
    }
}

/// The terminal decision of the macOS child-exit wait (ma-sdk-state r16 item
/// 4): NEITHER a changelist EV_ERROR nor a timeout is itself evidence of an
/// exit, and even a delivered NOTE_EXIT keeps the r15 item-2 discipline of a
/// confirming classifier read — the wait is satisfied only when a FRESH
/// disposition independently reads Gone or Zombie.
///
/// `event` therefore never sways the verdict (every outcome is confirmed the
/// same way), but threading the PARSED event through the caller is the fix
/// shape: the wait closure must distinguish a changelist EV_ERROR from a real
/// NOTE_EXIT instead of treating every `Ok(1)` as an exit. The injected tests
/// pin the full event×disposition table.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
fn kqueue_exit_confirmed(event: KqueueWaitOutcome, disposition: ChildDisposition) -> bool {
    match (event, disposition) {
        // A fresh Gone/Zombie classifier read confirms the wait however it
        // ended — a registration EV_ERROR, an elapsed bound, or a failed
        // syscall — matching the "re-check on timeout or error" contract.
        (_, ChildDisposition::Gone | ChildDisposition::Zombie) => true,
        // A delivered NOTE_EXIT paired with an inconclusive disposition, a
        // changelist EV_ERROR (never an exit), an elapsed bound, or a failed
        // wait can none of them confirm death on their own.
        (
            KqueueWaitOutcome::NoteExit
            | KqueueWaitOutcome::ChangeError(_)
            | KqueueWaitOutcome::BoundElapsed
            | KqueueWaitOutcome::WaitFailed,
            _,
        ) => false,
    }
}

#[cfg(all(any(test, feature = "test-stub"), unix, target_os = "linux"))]
fn wait_child_exit_blocking(pid: i32, timeout: Duration, probe: Option<&EintrWaitProbe>) -> bool {
    // A pidfd is a pollable fd that becomes readable when the process EXITS
    // (the zombie transition included), even when it is not ours to reap, so
    // the wait parks in the kernel instead of polling /proc on a timer.
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::process::{Pid, PidfdFlags, pidfd_open};

    let Some(target) = Pid::from_raw(pid) else {
        return false;
    };
    let fd = match pidfd_open(target, PidfdFlags::empty()) {
        Ok(fd) => fd,
        // ESRCH: it is already gone. Anything else (ENOSYS on a pre-5.3
        // kernel, EPERM) has no event source here: one classifier read rather
        // than a sleep, and a live/unknown answer fails the bound honestly.
        Err(rustix::io::Errno::SRCH) => {
            return matches!(
                child_disposition(pid),
                ChildDisposition::Gone | ChildDisposition::Zombie
            );
        }
        Err(_) => return false,
    };
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    // One monotonic deadline across EINTR restarts (r16 item 2): each poll gets
    // only the time remaining, so a signal storm can neither fail the wait
    // early nor renew the bound.
    let waited = wait_with_eintr_retry(timeout, probe, |remaining| {
        let Some(timespec) = Timespec::try_from(remaining).ok() else {
            return Ok(InterruptibleEventWait::TimedOut);
        };
        match poll(&mut fds, Some(&timespec)) {
            Ok(1) => Ok(InterruptibleEventWait::Fired(())),
            Ok(_) => Ok(InterruptibleEventWait::TimedOut),
            Err(rustix::io::Errno::INTR) => Err(std::io::Error::from(rustix::io::Errno::INTR)),
            Err(other) => Err(std::io::Error::from(other)),
        }
    });
    matches!(waited, Ok(InterruptibleEventWait::Fired(())))
        && matches!(
            child_disposition(pid),
            ChildDisposition::Gone | ChildDisposition::Zombie
        )
}

#[cfg(all(any(test, feature = "test-stub"), unix, not(target_os = "linux")))]
fn wait_child_exit_blocking(pid: i32, timeout: Duration, probe: Option<&EintrWaitProbe>) -> bool {
    // The macOS/BSD equivalent of the pidfd wait: an EVFILT_PROC/NOTE_EXIT
    // registration delivers when the watched process exits. nix's safe
    // kqueue wrapper is used because the workspace forbids `unsafe`; same-uid
    // processes (our own test children) are watchable though not reaped by us.
    use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};

    // ma-sdk-state r16 item 4: classify BEFORE arming. The pre-close SIGKILL
    // proofs call this on a child that has ALREADY exited and is an unreaped
    // zombie, and EVFILT_PROC/NOTE_EXIT is edge-triggered — it never replays a
    // death that happened before the EV_ADD landed, so the old arm parked for
    // its whole bound and panicked the proofs. Gone/Zombie is success without
    // a syscall (the Linux pidfd arm needs no equivalent: pidfd readiness is
    // level-triggered).
    if kqueue_pre_arm_move(child_disposition(pid)) == KqueuePreArm::AlreadyConfirmed {
        return true;
    }

    let Ok(kq) = Kqueue::new() else {
        // No event source: a fresh classifier read is the only evidence left.
        return matches!(
            child_disposition(pid),
            ChildDisposition::Gone | ChildDisposition::Zombie
        );
    };
    let change = KEvent::new(
        pid as nix::libc::uintptr_t,
        EventFilter::EVFILT_PROC,
        EventFlag::EV_ADD | EventFlag::EV_ENABLE,
        FilterFlag::NOTE_EXIT,
        0,
        0,
    );
    let mut events = [KEvent::new(
        0,
        EventFilter::EVFILT_PROC,
        EventFlag::empty(),
        FilterFlag::empty(),
        0,
        0,
    )];
    // EINTR retries against the ONE monotonic deadline with the remaining
    // time (r16 item 2). On Ok(1) the returned entry is PARSED: a changelist
    // EV_ERROR (errno in data) is a failed registration, NOT a NOTE_EXIT
    // (r16 item 4 — the old arm treated every Ok(1) as an exit).
    let waited = wait_with_eintr_retry::<KqueueWaitOutcome>(timeout, probe, |remaining| {
        let deadline = Some(nix::libc::timespec {
            tv_sec: remaining.as_secs() as nix::libc::time_t,
            tv_nsec: remaining.subsec_nanos() as nix::libc::c_long,
        });
        match kq.kevent(&[change], &mut events, deadline) {
            Ok(0) => Ok(InterruptibleEventWait::TimedOut),
            Ok(1) => {
                let entry = &events[0];
                let outcome = if entry.flags().contains(EventFlag::EV_ERROR) {
                    KqueueWaitOutcome::ChangeError(entry.data() as i32)
                } else {
                    KqueueWaitOutcome::NoteExit
                };
                Ok(InterruptibleEventWait::Fired(outcome))
            }
            // eventlist has exactly one slot; the kernel cannot report more.
            Ok(_) => Ok(InterruptibleEventWait::TimedOut),
            Err(nix::errno::Errno::EINTR) => Err(std::io::Error::from(nix::errno::Errno::EINTR)),
            Err(_) => Ok(InterruptibleEventWait::Fired(KqueueWaitOutcome::WaitFailed)),
        }
    });
    // Terminal read (r16 item 4): re-classify on EVERY outcome — a delivered
    // NOTE_EXIT, a changelist EV_ERROR, an elapsed bound, or a failed wait —
    // and confirm only Gone/Zombie.
    let event = match waited {
        Ok(InterruptibleEventWait::Fired(event)) => event,
        Ok(InterruptibleEventWait::TimedOut) => KqueueWaitOutcome::BoundElapsed,
        Err(_) => KqueueWaitOutcome::WaitFailed,
    };
    kqueue_exit_confirmed(event, child_disposition(pid))
}

/// One member of a process group as read from the host process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GroupMember {
    pub pid: i32,
    /// Dead but not yet reaped by its parent: zombie (`Z`), a process stuck in
    /// exit (`E` on macOS, `X`/`x` on Linux), or any later dead-state letter.
    pub zombie: bool,
}

/// Whether a process-table state letter names a process that is already dead
/// and only needs (or will need) reaping.
///
/// The demo defect (native-carrier-3): macOS reports a SIGKILLed session
/// leader as `?Es` — `E` = "exiting", a session leader stuck in the kernel's
/// exit path — and the old classifier only recognised `Z`, so the ladder named
/// it in `stop-incomplete` and it lingered for minutes as a child of the Node.
/// Linux's analogous letters are `Z` (zombie) and `X`/`x` (dead); BSD/macOS
/// adds `E` (exiting). A process in any of these cannot be killed again or
/// make progress; the only honest question left is when its parent reaps it.
#[must_use]
pub(crate) fn state_is_dead(state: &str) -> bool {
    // macOS `stat` prefixes `?` when the process has no controlling terminal —
    // the demo row `?Es` is that marker, then `E` exiting, then `s` session
    // leader. Linux never emits it. Skip it before reading the state letter;
    // subsequent chars (`s`, `+`, `l`) are flags and are ignored.
    let state = state.strip_prefix('?').unwrap_or(state);
    matches!(state.chars().next(), Some('Z' | 'X' | 'x' | 'E'))
}

/// List group members with their liveness state.
///
/// A zombie is a process that has died and is only waiting for its parent to
/// `wait`. `killpg(0)` still counts one, but the ladder must not: the Node is
/// the parent of the group leader and is about to reap it, and a grandchild
/// zombie has already been reparented to init, which reaps it.
#[cfg(unix)]
fn group_members(pgid: i32) -> Vec<GroupMember> {
    #[cfg(target_os = "linux")]
    {
        proc_group_members(pgid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        ps_group_members(pgid)
    }
}

/// Whether the group contains a member that is still actually running.
///
/// Reaps (via the supplied callback) are performed by the caller; this answers
/// the zombie-aware question `killpg(0)` cannot: a dead-but-unreaped child is
/// not a survivor, and naming it in `stop-incomplete` is the defect that
/// produced `the process group outlived SIGKILL` against a process already in
/// state `E`/`Z`.
#[must_use]
pub(crate) fn group_has_live_member(pgid: i32) -> bool {
    #[cfg(unix)]
    {
        group_members(pgid).iter().any(|member| !member.zombie)
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

/// `/proc/<pid>/stat` scan: exact, no `ps` dependency (Linux).
///
/// `/proc/stat` field 2 (`comm`) is wrapped in parentheses and may itself
/// contain spaces or parentheses, so parsing starts after the *last* `)`.
/// The following fields are: state(3) ppid(4) pgrp(5) …
#[cfg(target_os = "linux")]
fn proc_group_members(pgid: i32) -> Vec<GroupMember> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        // /proc unreadable: fail toward the conservative killpg answer.
        return if group_alive(pgid) {
            vec![GroupMember {
                pid: pgid,
                zombie: false,
            }]
        } else {
            Vec::new()
        };
    };
    let mut members = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start()) else {
            continue;
        };
        if let Some(member) = parse_proc_stat_line(pgid, pid, after_comm) {
            members.push(member);
        }
    }
    members
}

/// Parse the post-`comm` tail of a `/proc/<pid>/stat` line:
/// `state ppid pgrp …`. Exposed for tests; [`proc_group_members`] does the
/// directory scan.
#[cfg(any(test, target_os = "linux"))]
pub(crate) fn parse_proc_stat_line(pgid: i32, pid: i32, after_comm: &str) -> Option<GroupMember> {
    let mut fields = after_comm.split_whitespace();
    let state = fields.next()?;
    let _ppid = fields.next();
    let group = fields.next()?;
    (group.parse::<i32>().ok() == Some(pgid)).then_some(GroupMember {
        pid,
        zombie: state_is_dead(state),
    })
}

/// `ps -eo pgid=,pid=,stat=` scan for non-Linux unixes (macOS zombies are `Z`,
/// a process stuck exiting is `E`).
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
#[cfg_attr(test, allow(dead_code))]
fn ps_group_members_via_ps(pgid: i32) -> Vec<GroupMember> {
    let output = std::process::Command::new("ps")
        .args(["-eo", "pgid=,pid=,stat="])
        .output();
    match output {
        Ok(out) if out.status.success() => parse_ps_table(pgid, &out.stdout),
        _ => {
            if group_alive(pgid) {
                vec![GroupMember {
                    pid: pgid,
                    zombie: false,
                }]
            } else {
                Vec::new()
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_group_members(pgid: i32) -> Vec<GroupMember> {
    ps_group_members_via_ps(pgid)
}

/// Parse `ps -eo pgid=,pid=,stat=` output into the members of `pgid`.
///
/// Split out from the `ps` invocation so the macOS `?Es` classification has a
/// unit test on every host: the parsing is the platform-specific part, not the
/// `ps` call.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
pub(crate) fn parse_ps_table(pgid: i32, bytes: &[u8]) -> Vec<GroupMember> {
    let mut members = Vec::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        let mut parts = line.split_whitespace();
        let Some(group) = parts.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        let Some(pid) = parts.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        let state = parts.next().unwrap_or("");
        if group == pgid {
            members.push(GroupMember {
                pid,
                zombie: state_is_dead(state),
            });
        }
    }
    members
}

/// Wait up to `grace` for the group to disappear.
///
/// Before every liveness check the caller's reaper runs: the group leader is
/// this Node's direct child, a `SIGKILL`ed child sits in `Z` until reaped, and
/// an unreaped zombie still answers `killpg(0)` — exactly the false survivor
/// the demo found.
async fn settled<R>(pgid: i32, grace: Duration, reap: &mut R) -> bool
where
    R: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        reap();
        if !group_has_live_member(pgid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            // One last reap before giving up; the leader may have exited in the
            // final poll window.
            reap();
            return !group_has_live_member(pgid);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Drive `SIGINT` → `SIGHUP` → `SIGKILL` against `pgid` (§5.3).
///
/// `close_master` is invoked between the `SIGINT` and `SIGHUP` rungs: §5.3
/// step 2 pairs the hangup with closing the master fd, because a reader still
/// holding the master keeps the slave's other end open and a shell waiting on
/// input never notices the hangup.
///
/// `reap` collects this Node's dead direct child (the group leader) and
/// returns whether it did. It runs before every liveness check: a
/// `SIGKILL`ed child is a zombie until waited, and a zombie still answers
/// `killpg(0)` — without the reap the ladder reported a false
/// `stop-incomplete` against a process already dead in `E`/`Z`.
///
/// Never returns an error for "the group would not die" — that is a
/// [`StopOutcome`] with `group_gone: false`, which the caller journals as
/// `stop-incomplete`. An `Err` here means the *signal call itself* failed,
/// which is a bug in the caller's pgid, not a stubborn process.
pub(super) async fn stop_group<F, R>(
    pgid: i32,
    mut close_master: F,
    reap: &mut R,
) -> DriverResult<StopOutcome>
where
    F: FnMut(),
    R: FnMut() -> bool,
{
    #[cfg(not(unix))]
    {
        let _ = (&mut close_master, pgid, reap);
        return Err(DriverError::CapabilityUnsupported(
            "the stop ladder requires unix process groups".into(),
        ));
    }
    #[cfg(unix)]
    {
        use nix::sys::signal::Signal;

        if pgid <= 0 {
            close_master();
            return Ok(StopOutcome::already_gone());
        }
        // Collect an already-dead leader first: a process that exited on its
        // own is a zombie its parent never waited, and the zombie still
        // answers killpg. When nothing live remains there is no ladder to run.
        reap();
        if !group_has_live_member(pgid) {
            close_master();
            return Ok(StopOutcome::already_gone());
        }

        signal_group(pgid, Signal::SIGINT)?;
        if settled(pgid, GRACE_INT, reap).await {
            close_master();
            return Ok(StopOutcome {
                rung: StopRung::Interrupt,
                group_gone: true,
                pgid: Some(pgid),
                survivors: Vec::new(),
                observation_unknown: false,
            });
        }

        // §5.3 step 2: the hangup and the master fd go together.
        close_master();
        signal_group(pgid, Signal::SIGHUP)?;
        if settled(pgid, GRACE_HUP, reap).await {
            return Ok(StopOutcome {
                rung: StopRung::Hangup,
                group_gone: true,
                pgid: Some(pgid),
                survivors: Vec::new(),
                observation_unknown: false,
            });
        }

        signal_group(pgid, Signal::SIGKILL)?;
        let gone = settled(pgid, GRACE_KILL, reap).await;
        Ok(StopOutcome {
            rung: StopRung::Kill,
            group_gone: gone,
            pgid: Some(pgid),
            // Only genuinely-live members are survivors: a zombie is gone,
            // whether this caller reaped it or init did after reparenting.
            survivors: if gone {
                Vec::new()
            } else {
                live_survivors(pgid)
            },
            observation_unknown: false,
        })
    }
}

/// How often the stop-incomplete termination watcher rechecks the survivor
/// process group (c-usagefu r13 item 3).
pub(crate) const TERMINATION_WATCH_POLL: Duration = Duration::from_millis(250);

/// Upper bound a stop-incomplete teardown watches a survivor that outlived
/// SIGKILL before giving up. SIGKILL cannot be caught, so a process still here
/// after the ladder is blocked in the kernel (uninterruptible IO) or an
/// unreaped zombie its parent cannot collect; it almost always clears within
/// seconds, but the watch is bounded so a wedged host cannot leave a polling
/// task on this Node forever. Past the bound the guards and process identity
/// move to the Node-owned pending-termination registry (c-usagefu r14 item 3,
/// Ruling W1) instead of being dropped: the interval closes only after
/// confirmed exit, while a Node shutdown still drops them without recording a
/// false close.
pub(super) const TERMINATION_WATCH_BOUND: Duration = Duration::from_secs(300);

#[cfg(test)]
static TEST_WATCH_BOUND_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test seam: shorten the per-close bound so a regression can drive the
/// expiry→registry transfer without waiting 300 s. `None` restores the
/// production bound.
#[cfg(test)]
pub(crate) fn set_test_watch_bound(d: Option<Duration>) {
    TEST_WATCH_BOUND_MS.store(
        d.map(|x| x.as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(0),
        std::sync::atomic::Ordering::SeqCst,
    );
}

/// The per-close bound a watcher uses (test seam or the production 300 s).
pub(crate) fn termination_watch_bound() -> Duration {
    #[cfg(test)]
    {
        let ms = TEST_WATCH_BOUND_MS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    TERMINATION_WATCH_BOUND
}

/// Whether a stop-incomplete survivor identity is still running. c-usagefu r15
/// item 5 (COORDINATOR RULING F): a named survivor is a (pid, start time)
/// identity — a reused pid with a different start time means the ORIGINAL
/// process exited, and an unreadable process state is Unknown (kept), never
/// death.
///
/// c-usagefu r16 item 2 (Ruling F'): when the pre-signal SNAPSHOT itself could
/// not be read (`observation_unknown`) no descendant identity was ever
/// captured, so neither the group signal nor the survivor probes can speak for
/// the tree: Unknown, always kept.
///
/// c-usagefu r16 item 4: surviving membership is decided ONLY from the
/// captured identities — never from the signalled group NUMBER. After the
/// original group exits the kernel can reuse its pgid for an unrelated
/// session; a matching live number used to keep the interval live forever (and
/// block same-cwd discovery) even when every captured process was gone. `pgid`
/// is retained on the identity for diagnostics, but it cannot keep an entry
/// alive. With no captured identity and no Unknown observation the entry is
/// gone.
pub(crate) fn termination_identity_alive(
    pgid: Option<i32>,
    survivors: &[ProcessIdentity],
    observation_unknown: bool,
) -> bool {
    if observation_unknown {
        return true;
    }
    // Deliberately ignored for the liveness decision (see above).
    let _ = pgid;
    survivors.iter().copied().any(process_identity_alive)
}

/// One process the stop ladder could not confirm dead, identified per Ruling F
/// by (pid, start time). `start_ticks` is the `/proc/<pid>/stat` starttime
/// field captured BEFORE the ladder (clock ticks since boot); `None` on hosts
/// or read failures where no start-time baseline exists, in which case the
/// entry can never be start-time proven gone.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: i32,
    pub(crate) start_ticks: Option<u64>,
}

/// Whether one named process identity is still running.
///
/// COORDINATOR RULING F (fail closed): only CONFIRMED exit closes.
/// * a zombie/exiting state is dead (an unreaped corpse cannot write);
/// * a start-time mismatch against the captured baseline is a REUSED pid —
///   the original process exited even though the pid is live again;
/// * a vanished pid is dead only when signal 0 confirms `ESRCH`;
/// * every other unreadable state (`/proc` permission/IO errors, an `EPERM`
///   signal probe, absent `/proc` data) is UNKNOWN → reported alive, so the
///   registry keeps the interval instead of closing on a guess.
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn process_identity_alive(identity: ProcessIdentity) -> bool {
    let stat = match read_proc_stat(identity.pid) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A hidden /proc can make a live pid read NotFound too, so death
            // needs the confirming ESRCH (Ruling F).
            return !matches!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(identity.pid), None),
                Err(nix::errno::Errno::ESRCH)
            );
        }
        Err(_) => return true, // permission / resource exhaustion: Unknown
    };
    let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start()) else {
        return process_alive(identity.pid);
    };
    let Some((state, start_ticks)) = parse_proc_state_start(after_comm) else {
        return process_alive(identity.pid);
    };
    if state_is_dead(state) {
        return false;
    }
    if let Some(expected) = identity.start_ticks
        && start_ticks != expected
    {
        // The pid was reused: a different start time means the captured
        // process exited.
        return false;
    }
    true
}

/// Capture a pid's starttime baseline (stat field 22, clock ticks since boot)
/// for a later start-time identity comparison (Ruling F). None when the pid
/// is gone or its stat cannot be read/parsed.
#[cfg(target_os = "linux")]
pub(crate) fn process_start_ticks(pid: i32) -> Option<u64> {
    let stat = read_proc_stat(pid).ok()?;
    let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start())?;
    parse_proc_state_start(after_comm).map(|(_, ticks)| ticks)
}

/// Test seam: force every `/proc/<pid>/stat` READ to fail as a permission
/// error, so Ruling F's unreadable-state branch is exercised deterministically
/// (c-usagefu r15 item 5). Process-global; termination tests serialize on the
/// termination test lock.
#[cfg(all(test, target_os = "linux"))]
static TEST_PROC_READ_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn set_test_proc_read_failure(force: bool) {
    TEST_PROC_READ_FAILURE.store(force, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(all(test, target_os = "linux"))]
fn read_proc_stat(pid: i32) -> std::io::Result<String> {
    if TEST_PROC_READ_FAILURE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "injected /proc read failure",
        ));
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
}

#[cfg(all(not(test), target_os = "linux"))]
fn read_proc_stat(pid: i32) -> std::io::Result<String> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
}

/// Test seam: make the `/proc` DIRECTORY scan (the pre-signal snapshot and the
/// post-ladder rescan) for ONE signalled group fail as a permission error, so
/// Ruling F's unreadable-table branch is exercised deterministically WITHOUT
/// also failing the ladder's own group scan, the per-pid identity probes, or —
/// crucially — a bystander driver's close running in the same parallel test
/// process. The seam is keyed to the test's own pgid: `Some(pgid)` arms the
/// fault for that group, `None` restores real reads (c-usagefu r16 item 2).
#[cfg(all(test, target_os = "linux"))]
static TEST_PROC_SCAN_FAILURE_PGID: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(-1);

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn set_test_proc_scan_failure(armed_for_pgid: Option<i32>) {
    TEST_PROC_SCAN_FAILURE_PGID.store(
        armed_for_pgid.map(i64::from).unwrap_or(-1),
        std::sync::atomic::Ordering::SeqCst,
    );
}

#[cfg(all(test, target_os = "linux"))]
fn test_proc_scan_fails_for(pgid: i32) -> bool {
    TEST_PROC_SCAN_FAILURE_PGID.load(std::sync::atomic::Ordering::SeqCst) == i64::from(pgid)
}

#[cfg(target_os = "linux")]
fn read_proc_dir() -> std::io::Result<std::fs::ReadDir> {
    std::fs::read_dir("/proc")
}

/// Parse the state letter and the starttime field (22nd stat field, the 20th
/// after `comm`) from a `/proc/<pid>/stat` tail.
#[cfg(target_os = "linux")]
fn parse_proc_state_start(after_comm: &str) -> Option<(&str, u64)> {
    let mut fields = after_comm.split_whitespace();
    let state = fields.next()?;
    // In the tail after `comm`: ppid(0) pgrp(1) session(2) tty_nr(3)
    // tpgid(4) flags(5) minflt(6) cminflt(7) majflt(8) cmajflt(9)
    // utime(10) stime(11) cutime(12) cstime(13) priority(14) nice(15)
    // num_threads(16) itrealvalue(17) starttime(18) — stat field 22.
    let starttime: u64 = fields.nth(18)?.parse().ok()?;
    Some((state, starttime))
}

/// Whether a single pid is still running AND not merely an unreaped zombie.
///
/// `kill(pid, 0)` returns success for a zombie, which must not keep a census
/// interval live; read the `/proc` state letter first (zombie/exiting states
/// are dead — [`state_is_dead`]), and treat an unreadable state as Unknown
/// (Ruling F): it stays alive rather than being guessed dead. The production
/// stop path probes identities through [`process_identity_alive`]; this
/// convenience is used by the Linux-only tree tests.
#[cfg(all(test, target_os = "linux"))]
#[must_use]
pub(crate) fn process_running(pid: i32) -> bool {
    process_identity_alive(ProcessIdentity {
        pid,
        start_ticks: None,
    })
}

/// Map one observed non-`/proc` child disposition to the pid-only,
/// Unknown-strength SURVIVOR decision (COORDINATOR RULING F', c-usagefu r16
/// item 5).
///
/// macOS has no safe start-time API (unsafe libproc FFI is forbidden), so a
/// survivor identity is pid-only. The entry stays ALIVE unless the pid is
/// CONFIRMED gone — only [`ChildDisposition::Gone`], which requires a
/// signal-0 `ESRCH`:
///
/// * [`ChildDisposition::Zombie`] still answers signal 0 (it is not ESRCH)
///   and a pid-only identity cannot prove the corpse is the captured process,
///   so it is kept;
/// * a reused pid answers signal 0 as well — kept (conservative, accepted by
///   F');
/// * [`ChildDisposition::Unknown`] and [`ChildDisposition::Alive`] are kept.
///
/// Split out so the decision is unit-tested with injected outcomes on every
/// host, including macOS.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
pub(crate) fn macos_survivor_alive(disposition: ChildDisposition) -> bool {
    !matches!(disposition, ChildDisposition::Gone)
}

/// Non-Linux unix: no `/proc` start time. The identity is pid-only and
/// Unknown-strength; [`process_identity_alive`] closes ONLY on a confirmed
/// signal-0 `ESRCH` (Ruling F', c-usagefu r16 item 5). The struct itself is the
/// single `#[cfg(unix)]` definition above (c-usagefu r16 item 1: the two
/// overlapping unix definitions made macOS fail to compile).
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn process_identity_alive(identity: ProcessIdentity) -> bool {
    let _ = identity.start_ticks;
    macos_survivor_alive(child_disposition(identity.pid))
}

/// Build the stop-ladder survivor identity for `pid`: pid plus the captured
/// start-time baseline where `/proc` provides one (Ruling F).
#[cfg(target_os = "linux")]
pub(crate) fn survivor_identity(pid: i32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_ticks: process_start_ticks(pid),
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn survivor_identity(pid: i32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_ticks: None,
    }
}

/// Non-unix hosts name survivors by pid alone; nothing closes except via the
/// platform's own exit detection.
#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: i32,
}

#[cfg(not(unix))]
pub(crate) fn survivor_identity(pid: i32) -> ProcessIdentity {
    ProcessIdentity { pid }
}

#[cfg(not(unix))]
pub(crate) fn process_identity_alive(_identity: ProcessIdentity) -> bool {
    false
}

/// One process captured in the pre-signal tree snapshot: its pid, process
/// group (a setsid descendant has a group of its own), and its `/proc` start
/// time captured BEFORE the ladder. Linux-only: both the snapshot and the
/// post-ladder rescan read `/proc`.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct TreeProc {
    pub(crate) pid: i32,
    /// The post-ladder rescan reads each member's CURRENT group; this
    /// snapshot group is retained for the tree tests' pre-signal assertions.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) pgrp: i32,
    /// Start-time baseline captured in the SAME pre-signal pass. If the member
    /// vanishes during the ladder and its pid is later reused, a mismatch
    /// against this baseline proves the captured process exited (Ruling F').
    pub(crate) start_ticks: Option<u64>,
}

/// What a `/proc` process-table read established for the pre-signal snapshot
/// and the post-ladder rescan (COORDINATOR RULING F', c-usagefu r16 item 2).
///
/// Every process observation is either Established or Unknown: a table that
/// cannot be read (ENOENT aside, resource exhaustion, permissions) is
/// **Unknown** — it must never be read as "no survivors", never close an
/// interval, and never drop a captured identity. Callers retain every captured
/// identity; only a later confirmed death (or a Linux start-time mismatch)
/// discards one.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProcessTableObservation<T> {
    /// The table was read and every relevant row accounted for; `T` is the
    /// established result.
    Established(T),
    /// The table itself could not be read. The caller fails closed.
    Unknown,
}

/// Snapshot the descendant process tree of `root_pid` BEFORE the stop ladder
/// signals anything (c-usagefu r14 item 4).
///
/// A descendant that called `setsid` (a harness daemonising a child) is in its
/// OWN process group and escapes the group kill; after the group is dead it
/// would also be invisible to a ppid-rooted scan, because killing the leader
/// reparents the orphan to init before the scan runs. Enumerating the tree
/// BEFORE the signals makes every escaped descendant discoverable.
///
/// Transitive closure over `ppid` edges read in one `/proc` pass. The root
/// itself is included.
///
/// Returns [`ProcessTableObservation::Unknown`] when the process table itself
/// cannot be opened (Ruling F'): the caller has NO captured descendant and the
/// stop decision must fail closed rather than treating an unreadable table as
/// an empty tree.
#[cfg(target_os = "linux")]
pub(crate) fn snapshot_process_tree(root_pid: i32) -> ProcessTableObservation<Vec<TreeProc>> {
    // Test seam: a per-pgid injected unreadable table (c-usagefu r16 item 2).
    #[cfg(test)]
    if test_proc_scan_fails_for(root_pid) {
        return ProcessTableObservation::Unknown;
    }
    let Ok(entries) = read_proc_dir() else {
        return ProcessTableObservation::Unknown;
    };
    let mut rows: Vec<TreeProc> = Vec::new();
    let mut parents: Vec<(i32, i32)> = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start()) else {
            continue;
        };
        if let Some((ppid, pgrp, start_ticks)) = parse_proc_parent_group_start(after_comm) {
            rows.push(TreeProc {
                pid,
                pgrp,
                start_ticks,
            });
            parents.push((pid, ppid));
        }
    }
    // BFS over the ppid edges from the root.
    let mut descendants = vec![TreeProc {
        pid: root_pid,
        pgrp: root_pid,
        start_ticks: None,
    }];
    let mut frontier = vec![root_pid];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &parents {
            if *ppid == parent
                && !descendants.iter().any(|member| member.pid == *pid)
                && let Some(row) = rows.iter().find(|row| row.pid == *pid)
            {
                descendants.push(*row);
                frontier.push(*pid);
            }
        }
    }
    ProcessTableObservation::Established(descendants)
}

/// Parse the `state ppid pgrp` fields of a `/proc/<pid>/stat` tail, returning
/// `(ppid, pgrp)`. Used by the tree tests' current-group assertions.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn parse_proc_parent_group(after_comm: &str) -> Option<(i32, i32)> {
    parse_proc_parent_group_start(after_comm).map(|(ppid, pgrp, _)| (ppid, pgrp))
}

/// Parse a `/proc/<pid>/stat` tail into `(ppid, pgrp, starttime)`; starttime is
/// stat field 22 (clock ticks since boot), the Ruling F' start-time baseline.
#[cfg(target_os = "linux")]
pub(crate) fn parse_proc_parent_group_start(after_comm: &str) -> Option<(i32, i32, Option<u64>)> {
    let mut fields = after_comm.split_whitespace();
    let _state = fields.next()?;
    let ppid: i32 = fields.next()?.parse().ok()?;
    let pgrp: i32 = fields.next()?.parse().ok()?;
    // After state/ppid/pgrp the next field is session(2); starttime is tail
    // index 18 (stat field 22), i.e. nth(16) from here.
    let start_ticks = fields.nth(16).and_then(|value| value.parse().ok());
    Some((ppid, pgrp, start_ticks))
}

/// After the stop ladder, rescan the pre-signal snapshot members and return
/// the identities of those STILL LIVE whose CURRENT process group is no longer
/// the signalled group (c-usagefu r15 item 6).
///
/// The pre-signal snapshot alone misses a descendant that calls `setsid(2)`
/// DURING the grace (e.g. a SIGHUP handler that daemonises the signal-holding
/// process): its snapshot pgrp still MATCHES the signalled group. Reading each
/// snapshot member's CURRENT stat catches both forms — a pre-signal escape and
/// a mid-ladder group change.
///
/// COORDINATOR RULING F' (c-usagefu r16 item 2): Unknown is never "no
/// survivor". If the process table itself cannot be opened this returns
/// [`ProcessTableObservation::Unknown`] and the caller retains EVERY captured
/// identity. A member the table does not show (its stat is unreadable or the
/// row is absent) is retained as an Unknown-strength identity — a later
/// `kill(pid, 0) == ESRCH` is the only thing that proves it gone (hidden
/// `/proc` can otherwise hide a live pid). Only a confirmed dead state drops a
/// member here.
#[cfg(target_os = "linux")]
pub(crate) fn rescan_escaped_snapshot_members(
    tree: &[TreeProc],
    signalled_pgid: i32,
) -> ProcessTableObservation<Vec<ProcessIdentity>> {
    use std::collections::HashMap;
    // Test seam: a per-pgid injected unreadable table (c-usagefu r16 item 2).
    #[cfg(test)]
    if test_proc_scan_fails_for(signalled_pgid) {
        return ProcessTableObservation::Unknown;
    }
    let Ok(entries) = read_proc_dir() else {
        return ProcessTableObservation::Unknown;
    };
    // One /proc pass: pid -> (state, pgrp) for every process.
    let mut groups: HashMap<i32, (String, i32)> = HashMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start()) else {
            continue;
        };
        let mut fields = after_comm.split_whitespace();
        let Some(state) = fields.next() else {
            continue;
        };
        let _ppid = fields.next();
        let Some(pgrp) = fields.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        groups.insert(pid, (state.to_owned(), pgrp));
    }
    ProcessTableObservation::Established(
        tree.iter()
            .filter(|member| member.pid != signalled_pgid)
            .filter_map(|member| {
                // The identity carries the start-time baseline captured in the
                // pre-signal pass, never a fresh read: a reused pid must be
                // recognised by a start-time mismatch, not re-baselined into a
                // false survivor (Ruling F').
                let identity = ProcessIdentity {
                    pid: member.pid,
                    start_ticks: member.start_ticks,
                };
                match groups.get(&member.pid) {
                    // Established dead: the only direct observation that
                    // discards a captured member at this point.
                    Some((state, _)) if state_is_dead(state) => None,
                    // Established alive and OUTSIDE the signalled group: it
                    // escaped the group kill; the snapshot baseline proves the
                    // row is still the captured process.
                    Some((state, current_pgrp)) if *current_pgrp != signalled_pgid => {
                        debug_assert!(!state_is_dead(state));
                        Some(identity)
                    }
                    // Established alive still inside the signalled group.
                    Some(_) => None,
                    // The table opened but this member's row is absent or
                    // unreadable. Discard ONLY on confirmed death: a vanished
                    // pid that gives ESRCH, or a reused pid whose start time
                    // differs from the snapshot baseline. Anything less — a
                    // hidden `/proc`, EPERM — retains the identity and the
                    // watcher probes it again later (Ruling F').
                    None => process_identity_alive(identity).then_some(identity),
                }
            })
            .collect(),
    )
}

/// Pids in `pgid` that are still running (not zombies), for a
/// `stop-incomplete` diagnostic.
///
/// Diagnostic-only: a table that cannot be read weakens the message but never
/// turns a live process into a clean exit — [`group_has_live_member`] already
/// made that call on the liveness path.
fn live_survivors(pgid: i32) -> Vec<i32> {
    #[cfg(unix)]
    {
        group_members(pgid)
            .into_iter()
            .filter(|member| !member.zombie)
            .map(|member| member.pid)
            .collect()
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        Vec::new()
    }
}

/// How a PTY-hosted process ended. §5.5 requires both witnesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitEvidence {
    /// `wait()` returned a status.
    Code {
        /// Exit code.
        code: i32,
    },
    /// The process was killed by a signal.
    Signal {
        /// Signal name, e.g. `SIGKILL`.
        name: String,
    },
    /// The PTY master hit EOF and `wait()` had nothing to add.
    ///
    /// The slave is closed, so nothing is left to read or write; an instance in
    /// this state is over even though no status was collected.
    Eof,
}

impl ExitEvidence {
    /// Lifecycle state §5.5 maps this to: `exited` for a clean code, `failed`
    /// for anything else.
    #[must_use]
    pub fn lifecycle(&self) -> &'static str {
        match self {
            Self::Code { code: 0 } => "exited",
            // A non-zero code, a signal, or an EOF with no status are all
            // "this did not finish normally". §5.5 lists code 0 as the only
            // `exited`; collapsing the rest into it would hide crashes.
            _ => "failed",
        }
    }

    /// Short reason for the journal.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Code { code } => format!("native-exit-code-{code}"),
            Self::Signal { name } => format!("native-exit-signal-{name}"),
            Self::Eof => "native-exit-eof".to_owned(),
        }
    }

    /// Exit code, when one was collected.
    #[must_use]
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Code { code } => Some(*code),
            _ => None,
        }
    }

    /// Signal name, when the process was signalled.
    #[must_use]
    pub fn signal(&self) -> Option<&str> {
        match self {
            Self::Signal { name } => Some(name),
            _ => None,
        }
    }
}

/// Turn a `portable-pty` exit status into [`ExitEvidence`].
///
/// `portable-pty` flattens the wait status into a `u32` that follows the Unix
/// convention of `128 + signum` for a signalled process, and does not expose
/// `WIFSIGNALED`. Decoding the convention is therefore the only signal
/// information available; it is also what a shell reports, so the journal
/// agrees with what the operator would see by hand.
#[must_use]
pub fn evidence_from_status(status: &portable_pty::ExitStatus) -> ExitEvidence {
    let code = status.exit_code();
    // Above 128 the low bits are a signal number, not an exit code. 128 itself
    // is ambiguous (a real `exit 128`) and is left as a code.
    if (129..=192).contains(&code)
        && let Some(name) = signal_name(code as i32 - 128)
    {
        return ExitEvidence::Signal { name };
    }
    ExitEvidence::Code {
        code: i32::try_from(code).unwrap_or(i32::MAX),
    }
}

/// Name for a signal number, for the journal.
fn signal_name(signum: i32) -> Option<String> {
    #[cfg(unix)]
    {
        nix::sys::signal::Signal::try_from(signum)
            .ok()
            .map(|signal| signal.as_str().to_owned())
    }
    #[cfg(not(unix))]
    {
        let _ = signum;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_code_is_exited_and_everything_else_is_failed() {
        // §5.5: only exit code 0 is `exited`. A crashed agent must not land in
        // the same bucket as one the user closed.
        assert_eq!(ExitEvidence::Code { code: 0 }.lifecycle(), "exited");
        assert_eq!(ExitEvidence::Code { code: 1 }.lifecycle(), "failed");
        assert_eq!(
            ExitEvidence::Signal {
                name: "SIGKILL".into()
            }
            .lifecycle(),
            "failed"
        );
        assert_eq!(ExitEvidence::Eof.lifecycle(), "failed");
    }

    #[test]
    fn the_reason_names_the_evidence_that_produced_it() {
        assert_eq!(
            ExitEvidence::Code { code: 3 }.reason(),
            "native-exit-code-3"
        );
        assert_eq!(
            ExitEvidence::Signal {
                name: "SIGTERM".into()
            }
            .reason(),
            "native-exit-signal-SIGTERM"
        );
        assert_eq!(ExitEvidence::Eof.reason(), "native-exit-eof");
    }

    #[test]
    fn a_status_above_128_decodes_as_the_signal_that_produced_it() {
        // portable-pty flattens wait status to 128 + signum and drops
        // WIFSIGNALED, so the convention is the only signal evidence there is.
        assert_eq!(
            evidence_from_status(&portable_pty::ExitStatus::with_exit_code(137)),
            ExitEvidence::Signal {
                name: "SIGKILL".into()
            },
            "137 = 128 + 9"
        );
        assert_eq!(
            evidence_from_status(&portable_pty::ExitStatus::with_exit_code(130)),
            ExitEvidence::Signal {
                name: "SIGINT".into()
            }
        );
    }

    #[test]
    fn an_ordinary_exit_code_is_not_mistaken_for_a_signal() {
        assert_eq!(
            evidence_from_status(&portable_pty::ExitStatus::with_exit_code(0)),
            ExitEvidence::Code { code: 0 }
        );
        assert_eq!(
            evidence_from_status(&portable_pty::ExitStatus::with_exit_code(1)),
            ExitEvidence::Code { code: 1 }
        );
        assert_eq!(
            evidence_from_status(&portable_pty::ExitStatus::with_exit_code(128)),
            ExitEvidence::Code { code: 128 },
            "128 is a legal `exit 128`, not a signal 0"
        );
    }

    #[test]
    fn rung_labels_are_the_signal_that_worked() {
        assert_eq!(StopRung::AlreadyGone.label(), "already-gone");
        assert_eq!(StopRung::Interrupt.label(), "sigint");
        assert_eq!(StopRung::Hangup.label(), "sighup");
        assert_eq!(StopRung::Kill.label(), "sigkill");
    }

    #[test]
    fn dead_state_letters_are_dead_in_every_os_spelling() {
        // The demo row was macOS `?Es`: E = exiting, session leader. Linux
        // reports `X`/`x` for dead and `Z` for zombie. Flag letters that
        // follow the state (`Es`, `Z+`, `Xsl`) must not change the verdict.
        for dead in ["Z", "Z+", "X", "Xsl", "x", "E", "Es", "?Es"] {
            assert!(state_is_dead(dead), "{dead:?} is dead-but-unreaped");
        }
        for live in ["R", "R+", "S", "Ss", "?Ss", "D", "Dl", "T", "t", "I", ""] {
            assert!(!state_is_dead(live), "{live:?} is still alive");
        }
    }

    #[test]
    fn a_synthetic_ps_table_classifies_an_exiting_macos_leader_as_dead() {
        // The exact native-carrier-3 evidence: pgid 16146, the claude leader
        // in state `?Es`, plus a zombie grandchild the same killpg sweep
        // reached. Neither may be named a survivor.
        let table = b"\
   16146  16146 ?Es\n\
   16146  16150 Z\n\
    9999   9999 ?Ss\n";
        let members = parse_ps_table(16146, table);
        assert_eq!(members.len(), 2);
        assert!(
            members.iter().all(|member| member.zombie),
            "the exiting leader and the zombie are both already dead: {members:?}"
        );
        assert!(
            !members.iter().any(|member| member.pid == 9999),
            "another group's leader must not be included"
        );
        // A live grandchild in another group never enters the table; verify a
        // live row inside the group keeps it alive separately below.
        assert!(
            !members.iter().any(|member| !member.zombie),
            "an all-dead table must read as gone even before waitpid"
        );
    }

    /// c-usagefu r16 item 5 (Ruling F') — the macOS/non-`/proc` SURVIVOR
    /// decision with injected dispositions. The identity there is pid-only and
    /// Unknown-strength: an entry closes ONLY when EVERY captured pid is
    /// confirmed gone (signal-0 `ESRCH` -> [`ChildDisposition::Gone`]). Gated
    /// to unix so it runs on macOS (and on Linux CI via the test-built helper).
    #[cfg(unix)]
    #[test]
    fn macos_survivor_decision_closes_only_on_confirmed_esrch_gone() {
        assert!(
            !macos_survivor_alive(ChildDisposition::Gone),
            "a confirmed signal-0 ESRCH (Gone) closes"
        );
        assert!(
            macos_survivor_alive(ChildDisposition::Zombie),
            "a zombie still answers signal 0 (not ESRCH); pid-only identity keeps it"
        );
        assert!(macos_survivor_alive(ChildDisposition::Alive));
        assert!(
            macos_survivor_alive(ChildDisposition::Unknown),
            "Unknown never closes an interval (Ruling F')"
        );

        // The registry aggregates identities with `any(alive)`: a single
        // non-Gone pid among confirmed-gone pids keeps the whole entry open,
        // and only when EVERY captured pid reads Gone does it close (a reused
        // pid that answers signal 0 is kept — conservative, accepted by F').
        let mixed = [
            ChildDisposition::Gone,
            ChildDisposition::Zombie,
            ChildDisposition::Gone,
        ];
        assert!(
            mixed.iter().copied().any(macos_survivor_alive),
            "one not-confirmed-gone pid keeps the entry open"
        );
        let reused_pid = [ChildDisposition::Gone, ChildDisposition::Alive];
        assert!(
            reused_pid.iter().copied().any(macos_survivor_alive),
            "a reused pid (signal-0 Ok) keeps it open"
        );
        let all_gone = [ChildDisposition::Gone, ChildDisposition::Gone];
        assert!(
            !all_gone.iter().copied().any(macos_survivor_alive),
            "every captured pid confirmed ESRCH-gone is the only close"
        );
    }

    /// macOS spells the demo's leader `?Es`, and none of those three
    /// characters is the run state `E`: the first char is the run state (`?`
    /// when the kernel state is not one of `IRSTUZ`), and `E`/`s` are *flags*
    /// — "trying to exit" and "session leader" (ps(1)). The classifier reads
    /// the row as dead either way, but only because it strips the `?` first,
    /// so pin the real spellings against a future "simplification".
    #[test]
    fn macos_flag_letters_do_not_change_the_run_state_reading() {
        // Real rows seen on the demo Mac during a forced delete.
        for exiting in ["?Es", "?E", "Es"] {
            assert!(
                state_is_dead(exiting),
                "{exiting:?}: a process trying to exit is dead-but-unreaped"
            );
        }
        // `s` alone is just a session leader, and a sleeping session leader is
        // the single most common row in the table — reading it as dead would
        // report every healthy agent as gone.
        for live in ["Ss", "Ss+", "?Ss", "S", "R+"] {
            assert!(!state_is_dead(live), "{live:?} is a live process");
        }
        // `E` as a trailing flag on a *live* run state still means the process
        // is on its way out and cannot be a survivor.
        assert!(state_is_dead("?Es"), "the exact demo row");
    }

    #[test]
    fn a_synthetic_ps_table_keeps_a_live_member_visible() {
        // One genuinely running grandchild keeps the group alive even when its
        // leader is already exiting.
        let table = b"\
   16146  16146 ?Es\n\
   16146  16151 Ss\n";
        let members = parse_ps_table(16146, table);
        assert!(
            members
                .iter()
                .any(|member| !member.zombie && member.pid == 16151)
        );
    }

    #[test]
    fn proc_stat_dead_letters_are_classified_from_the_post_comm_tail() {
        // `/proc/<pid>/stat` after the final `)`: state ppid pgrp …
        assert_eq!(
            parse_proc_stat_line(16146, 16146, "X 1 16146 16146 …").unwrap(),
            GroupMember {
                pid: 16146,
                zombie: true
            }
        );
        assert_eq!(
            parse_proc_stat_line(16146, 16147, "Z 16146 16146 …").unwrap(),
            GroupMember {
                pid: 16147,
                zombie: true
            }
        );
        let live = parse_proc_stat_line(16146, 16148, "S 16146 16146 …").unwrap();
        assert!(!live.zombie);
        assert!(parse_proc_stat_line(9999, 16146, "E 1 16146 …").is_none());
    }

    /// Spawn `script` as its own process-group leader and wait until it has
    /// signalled readiness by creating `ready`.
    ///
    /// Three things this has to get right, each of which silently produced a
    /// green-looking wrong answer while writing these tests:
    ///
    /// * `process_group(0)` rather than a post-spawn `setpgid` — once the child
    ///   has `exec`ed, changing its group is `EACCES`.
    /// * Wait for the readiness marker. Signalling a shell that has not yet
    ///   parsed its `trap` line kills it, and the ladder then reports the
    ///   SIGINT rung for a process that was never actually resistant.
    /// * Reap it. A SIGKILLed process stays a zombie until someone waits, and
    ///   a zombie is still a member of its process group, so an unreaped child
    ///   makes [`group_alive`] report a survivor that is already dead. In
    ///   production the §5.5 exit waiter is that reaper.
    #[cfg(unix)]
    async fn spawn_leader(script: &str, ready: &std::path::Path) -> i32 {
        use std::os::unix::process::CommandExt as _;
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]).process_group(0);
        let mut child = command.spawn().expect("spawn");
        let pid = i32::try_from(child.id()).expect("pid");
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fixture never signalled readiness"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pid
    }

    /// Spawn `script` as its own process-group leader, leaving the [`Child`]
    /// in the caller's hands: the caller decides when to reap, which is the
    /// whole point of the zombie tests.
    // Only the Linux zombie test needs an unreaped child: it is the one
    // platform where `/proc` lets the test observe the `Z` state directly.
    // Keeping the cfg wider than the caller makes this dead code on macOS,
    // which `-D warnings` rejects.
    #[cfg(all(unix, target_os = "linux"))]
    async fn spawn_leader_unreaped(
        script: &str,
        ready: &std::path::Path,
    ) -> (i32, std::process::Child) {
        use std::os::unix::process::CommandExt as _;
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]).process_group(0);
        let child = command.spawn().expect("spawn");
        let pid = i32::try_from(child.id()).expect("pid");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fixture never signalled readiness"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (pid, child)
    }

    /// Wait until `pid` is a zombie in `/proc` (Linux).
    #[cfg(all(unix, target_os = "linux"))]
    async fn wait_for_zombie(pid: i32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')
                        .map(|(_, rest)| rest.trim_start().starts_with('Z'))
                })
                .unwrap_or(false);
            if zombie {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{pid} never became a zombie"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A shell that traps SIGINT and SIGHUP and then stays resident.
    ///
    /// The `while` loop is load-bearing: with a single trailing `sleep`, `sh`
    /// exec-replaces itself with it and the traps leave with the shell.
    #[cfg(unix)]
    fn stubborn(ready: &std::path::Path) -> String {
        format!(
            "trap '' INT HUP; : > {}; while :; do sleep 0.1; done",
            ready.display()
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_group_that_ignores_sigint_is_escalated_until_it_is_gone() {
        // The whole point of the ladder: a process that catches SIGINT (every
        // agent TUI does) must still be stopped, and the outcome must name the
        // rung that actually worked rather than claiming the first one did.
        // `spawn_leader` reaps in a background thread, so the ladder's own
        // reaper is a no-op here.
        let dir = tempfile::tempdir().expect("tempdir");
        let ready = dir.path().join("ready");
        let pid = spawn_leader(&stubborn(&ready), &ready).await;

        let outcome = stop_group(pid, || {}, &mut || false)
            .await
            .expect("ladder runs");
        assert_eq!(
            outcome.rung,
            StopRung::Kill,
            "SIGINT and SIGHUP are trapped, so only SIGKILL can have worked"
        );
        assert!(outcome.group_gone, "SIGKILL cannot be caught");
        assert!(outcome.survivors.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cooperative_group_stops_at_the_first_rung() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ready = dir.path().join("ready");
        let script = format!(": > {}; while :; do sleep 0.1; done", ready.display());
        let pid = spawn_leader(&script, &ready).await;

        let outcome = stop_group(pid, || {}, &mut || false)
            .await
            .expect("ladder runs");
        assert_eq!(outcome.rung, StopRung::Interrupt);
        assert!(outcome.group_gone);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_group_that_has_already_left_is_not_an_error() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = i32::try_from(child.id()).expect("pid");
        let _ = child.wait();

        let mut closed = false;
        let outcome = stop_group(pid, || closed = true, &mut || false)
            .await
            .expect("ladder runs");
        assert_eq!(outcome.rung, StopRung::AlreadyGone);
        assert!(outcome.group_gone);
        assert!(closed, "the master is released even when nothing was there");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn every_descendant_of_the_group_leader_is_reaped_not_just_the_leader() {
        // §5.3's actual complaint: `child.kill()` SIGKILLs the login shell and
        // orphans the `claude` started inside it. A group-wide ladder must take
        // the grandchild with it.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("grandchild.pid");
        let ready = dir.path().join("ready");
        let script = format!(
            "sh -c 'trap \"\" INT HUP; echo $$ > {}; while :; do sleep 0.1; done' & {}",
            marker.display(),
            stubborn(&ready)
        );
        let pid = spawn_leader(&script, &ready).await;
        let grandchild = loop {
            if let Ok(text) = std::fs::read_to_string(&marker)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        let outcome = stop_group(pid, || {}, &mut || false)
            .await
            .expect("ladder runs");
        assert!(outcome.group_gone);
        assert!(
            !group_alive(grandchild),
            "the grandchild must not be orphaned; that is the defect §5.3 names"
        );
    }

    /// The defect from the live demo (native-pty-2c): a SIGKILLed child that
    /// nobody reaped is a zombie, still a member of its process group, and a
    /// zombie still answers `killpg(pgid, 0)`. The old ladder counted it as a
    /// survivor and emitted `stop-incomplete: the process group outlived
    /// SIGKILL` against a process that was already dead in `E`/`Z`.
    ///
    /// Here the child is killed, deliberately left unreaped, and then the
    /// ladder runs: the ladder reaps it itself and reports the group gone with
    /// zero survivors.
    #[cfg(all(unix, target_os = "linux"))]
    #[tokio::test]
    async fn an_unreaped_zombie_leader_is_reaped_by_the_ladder_not_named_a_survivor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ready = dir.path().join("ready");
        // `exec sleep` replaces the shell: one process in the group, so after
        // the SIGKILL the only member is its zombie.
        let script = format!(": > {}; exec sleep 30", ready.display());
        let (pid, mut child) = spawn_leader_unreaped(&script, &ready).await;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            Some(nix::sys::signal::Signal::SIGKILL),
        )
        .expect("kill leader");
        // Prove the premise: it really is a zombie, and killpg still sees it.
        wait_for_zombie(pid).await;
        assert!(group_alive(pid), "a zombie still answers killpg(0)");
        assert!(!group_has_live_member(pid), "...but it is not alive");

        // Cell rather than a captured `bool`: the closure owns what it moves,
        // and a copied flag would leave the assertion here looking at a value
        // the closure could never change.
        let reaped = std::cell::Cell::new(false);
        let outcome = stop_group(pid, || {}, &mut || {
            if reaped.get() {
                return true;
            }
            match child.try_wait() {
                Ok(Some(_status)) => {
                    reaped.set(true);
                    true
                }
                _ => false,
            }
        })
        .await
        .expect("ladder runs");
        assert!(reaped.get(), "the ladder reaps its own dead child");
        assert!(outcome.group_gone, "a zombie is not an outlived group");
        assert!(outcome.survivors.is_empty());
        assert_eq!(outcome.rung, StopRung::AlreadyGone);
    }

    /// r15 item 2: the event-driven child-exit wait parks for the kernel
    /// termination event and reads a SIGKILLed (still unreaped) child as
    /// gone/zombie, while a bound against a LIVE process returns false without
    /// a panic. The killed child is this test's own (the integration tests
    /// cover the non-child driver-child case); the pidfd/kqueue event fires at
    /// the zombie transition either way.
    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn the_event_wait_notices_a_sigkilled_child_and_times_out_a_live_one() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            // A live child: the bound must elapse and answer false.
            let mut live = std::process::Command::new("/bin/sh")
                .args(["-c", "exec sleep 30"])
                .spawn()
                .expect("spawn");
            let live_pid = i32::try_from(live.id()).expect("pid");
            let start = std::time::Instant::now();
            assert!(
                !wait_child_exit(live_pid, Duration::from_millis(200)).await,
                "a live child does not satisfy the exit wait"
            );
            assert!(start.elapsed() >= Duration::from_millis(180));

            // SIGKILL it, leave it unreaped: the kernel event (pidfd readable)
            // must fire while it is still a zombie.
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(live_pid),
                Some(nix::sys::signal::Signal::SIGKILL),
            )
            .expect("kill");
            assert!(
                wait_child_exit(live_pid, Duration::from_secs(5)).await,
                "the SIGKILLed child is seen gone/zombie from the exit event"
            );
            let _ = live.wait();
        });
    }

    /// r16 item 2 (injectable): three injected EINTR answers restart the wait
    /// with the shrinking remaining time and the fourth fires — the bound is one
    /// monotonic deadline, not four retries with the full timeout.
    #[cfg(unix)]
    #[test]
    fn eintr_restarts_the_exit_wait_with_the_remaining_time_until_it_fires() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const TIMEOUT: Duration = Duration::from_secs(60); // never elapses

        let calls = AtomicUsize::new(0);
        let outcome = wait_with_eintr_retry(TIMEOUT, None, |remaining| {
            let n = calls.fetch_add(1, Ordering::AcqRel);
            assert!(
                remaining <= TIMEOUT,
                "every attempt gets the remaining time at the single deadline"
            );
            if n > 0 {
                assert!(remaining < TIMEOUT, "a retry gets a strictly shorter bound");
            }
            match n {
                0..=2 => Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
                _ => Ok(InterruptibleEventWait::Fired(())),
            }
        })
        .expect("the wait itself never returns an error");
        assert_eq!(outcome, InterruptibleEventWait::Fired(()));
        assert_eq!(calls.load(Ordering::Acquire), 4);
    }

    /// r16 item 2 (injectable): uninterrupted EINTR answers keep restarting
    /// until the one deadline elapses — without sleeping the caller — and then
    /// answer TimedOut.
    #[cfg(unix)]
    #[test]
    fn eintr_retries_until_the_single_deadline_elapses() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const BOUND: Duration = Duration::from_millis(20);

        let calls = AtomicUsize::new(0);
        let start = std::time::Instant::now();
        let outcome = wait_with_eintr_retry::<()>(BOUND, None, |_| {
            calls.fetch_add(1, Ordering::AcqRel);
            Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
        })
        .expect("the wait itself never returns an error");
        assert_eq!(outcome, InterruptibleEventWait::TimedOut);
        assert!(
            start.elapsed() >= BOUND,
            "the retries continue until the bound elapses, not failing at the first EINTR"
        );
        assert!(
            calls.load(Ordering::Acquire) > 1,
            "a continuously interrupting wait retries against the same deadline"
        );
    }

    /// ma-sdk-state r19 item 2 (injectable, deterministic, no sleeps): the
    /// probe's deadline anchor is the EXACT instant the deadline is derived
    /// from, passed through unchanged — not a second clock sample the probe
    /// reads itself. The fb3c7aca implementation let the probe stamp a fresh
    /// `Instant::now()` a few dozen nanoseconds after the deadline's own
    /// sample; that gap is invisible to the real-ppoll timing proof because
    /// the interrupted syscall returns one to two milliseconds AFTER the
    /// deadline (verified: the old implementation passes the timing proof
    /// 20/20). A scripted clock settles the identity EXACTLY: the first
    /// sample is `t0`, the second — the loop's remaining calculation — is
    /// `t0 + SHIFT`; the fixed waiter samples once, hands that instant to
    /// the probe, and its closure sees `WAIT - SHIFT`, while the old probe
    /// stamped its own real-clock instant and fails the anchor equality by
    /// name.
    #[cfg(unix)]
    #[test]
    fn the_probe_anchor_is_the_exact_started_instant_the_deadline_derives_from() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        const WAIT: Duration = Duration::from_millis(400);
        const SHIFT: Duration = Duration::from_millis(10);

        let t0 = std::time::Instant::now();
        // Scripted clock: call 0 (the deadline's started instant) returns t0;
        // call 1 (the loop's first remaining calculation) returns t0+SHIFT;
        // every later call returns t0.
        let calls = AtomicUsize::new(0);
        let mut now = || {
            let n = calls.fetch_add(1, AtomicOrdering::AcqRel);
            match n {
                0 => t0,
                1 => t0 + SHIFT,
                _ => t0,
            }
        };

        let probe = EintrWaitProbe::default();
        let mut first_remaining = None;
        let outcome = wait_with_eintr_retry_clocked(WAIT, Some(&probe), &mut now, |remaining| {
            if first_remaining.is_none() {
                first_remaining = Some(remaining);
            }
            Ok(InterruptibleEventWait::Fired(()))
        })
        .expect("the wait itself never returns an error");
        assert_eq!(outcome, InterruptibleEventWait::Fired(()));
        // The anchor is the SAME instant the deadline derived from. The old
        // probe stamped a fresh std::time::Instant::now() and fails this
        // exact equality with the defect named.
        assert_eq!(
            probe.deadline_anchor(),
            Some(t0),
            "the probe must stamp the exact started instant the deadline is derived from"
        );
        // The loop's remaining time derives from the SECOND scripted sample
        // (t0+SHIFT): this pins the clock wiring — the started sample and the
        // remaining sample are distinct calls and neither is duplicated.
        assert_eq!(
            first_remaining,
            Some(WAIT - SHIFT),
            "the first wait receives the remaining time against the one started instant"
        );
        // Fixed shape: one started sample and one remaining sample before
        // the fired closure returns. The old probe's extra sample reads the
        // REAL clock (not this scripted one), so the count stays two under
        // both shapes — the anchor equality above is the discriminator.
        assert_eq!(
            calls.load(AtomicOrdering::Acquire),
            2,
            "one started sample and one remaining sample before the wait fires"
        );
    }

    /// r16 item 2 (injectable): any error other than EINTR propagates on the
    /// first attempt — an unexpected syscall failure still fails honestly.
    #[cfg(unix)]
    #[test]
    fn a_non_eintr_wait_error_propagates_immediately() {
        let error = wait_with_eintr_retry::<()>(Duration::from_secs(60), None, |_| {
            Err(std::io::Error::other("boom"))
        })
        .expect_err("the non-EINTR error propagates");
        assert_eq!(error.to_string(), "boom");
    }

    /// r16 item 2 (injectable): the first attempt may already see an elapsed
    /// bound and answers TimedOut without ever entering the syscall.
    #[cfg(unix)]
    #[test]
    fn a_zero_remaining_bound_times_out_without_a_wait() {
        let outcome = wait_with_eintr_retry::<()>(
            Duration::ZERO,
            None,
            |_| -> std::io::Result<InterruptibleEventWait<()>> {
                unreachable!("no wait attempt happens after the deadline is gone")
            },
        )
        .expect("the wait itself never returns an error");
        assert_eq!(outcome, InterruptibleEventWait::<()>::TimedOut);
    }

    /// r16 item 4 (injectable; the coordinator also runs this on macOS): the
    /// pre-arm move and the full kqueue-outcome × disposition decision table.
    /// The kqueue syscall arm itself is macOS/BSD-only, but these rules are
    /// pure and must hold on every unix target.
    #[cfg(unix)]
    #[test]
    fn the_macos_exit_wait_confirms_zombies_before_arming_and_reads_ev_error() {
        use ChildDisposition::{Alive, Gone, Unknown, Zombie};
        use KqueueWaitOutcome::{BoundElapsed, ChangeError, NoteExit, WaitFailed};

        // Pre-arm: an already-dead child is confirmed WITHOUT touching
        // kqueue — the edge-triggered NOTE_EXIT never replays for a zombie
        // that died before the EV_ADD (the r16 item 4 defect); an Alive or
        // Unknown child is registered and watched.
        assert_eq!(kqueue_pre_arm_move(Gone), KqueuePreArm::AlreadyConfirmed);
        assert_eq!(kqueue_pre_arm_move(Zombie), KqueuePreArm::AlreadyConfirmed);
        assert_eq!(kqueue_pre_arm_move(Alive), KqueuePreArm::RegisterAndWait);
        assert_eq!(kqueue_pre_arm_move(Unknown), KqueuePreArm::RegisterAndWait);

        // The ESRCH a dying child leaves in a changelist EV_ERROR must not be
        // read as an exit by itself: with an inconclusive disposition the
        // answer is false (the old arm treated every Ok(1) as an exit).
        assert!(!kqueue_exit_confirmed(
            ChangeError(nix::libc::ESRCH),
            Unknown
        ));
        assert!(!kqueue_exit_confirmed(ChangeError(nix::libc::EPERM), Alive));
        // …but the SAME changelist EV_ERROR confirms once a fresh classifier
        // read independently says Gone/Zombie ("re-check on error").
        assert!(kqueue_exit_confirmed(ChangeError(nix::libc::ESRCH), Gone));
        assert!(kqueue_exit_confirmed(ChangeError(0), Zombie));

        // A real NOTE_EXIT keeps the r15 item-2 confirming classifier read:
        // the event alone with an Alive/Unknown disposition is not enough.
        assert!(kqueue_exit_confirmed(NoteExit, Gone));
        assert!(kqueue_exit_confirmed(NoteExit, Zombie));
        assert!(!kqueue_exit_confirmed(NoteExit, Alive));
        assert!(!kqueue_exit_confirmed(NoteExit, Unknown));

        // The bound elapsing (the exact pre-close-proof hang the fix removes)
        // and a failed wait are satisfied by Gone/Zombie alone, never by an
        // Alive/Unknown read.
        assert!(kqueue_exit_confirmed(BoundElapsed, Zombie));
        assert!(!kqueue_exit_confirmed(BoundElapsed, Alive));
        assert!(kqueue_exit_confirmed(WaitFailed, Gone));
        assert!(!kqueue_exit_confirmed(WaitFailed, Unknown));
    }

    /// r16 item 2, Linux: a signal delivered to the WAITING thread while the
    /// pidfd wait is parked interrupts the syscall (EINTR); the retry keeps
    /// waiting against the SAME monotonic deadline and answers false only when
    /// the live child's bound elapses.
    ///
    /// A signal only produces a userspace EINTR when CAUGHT, and a thread-
    /// directed signal needs a pthread handle: the workspace forbids `unsafe`,
    /// so the empty handler is installed through `signal-hook`'s safe
    /// registration and delivery is `pthread_kill(3)` (nix's safe wrapper).
    /// `ppoll(2)` returns EINTR even though the registry installs SA_RESTART
    /// (poll is never auto-restarted per POSIX). The pre-fix waiter failed the
    /// wait at the interruption (~a few ms in); the fixed waiter continues to
    /// the bound.
    #[cfg(all(unix, target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn an_interrupted_active_exit_wait_continues_until_the_deadline() {
        use nix::sys::pthread::{Pthread, pthread_kill, pthread_self};
        use nix::sys::signal::Signal;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

        /// The bound: short enough for a hot EINTR retry loop, long enough to
        /// separate the old fail-immediate behavior from a continued wait.
        const WAIT: Duration = Duration::from_millis(400);
        /// ppoll(2) on x86_64.
        const PPOLL_NR: i32 = 271;

        /// ACKNOWLEDGE the wait is ACTIVELY parked in ppoll (no sleep): read
        /// the waiter's syscall file until it names ppoll, bounded by the
        /// caller's named deadline.
        fn acknowledge_waiter_parked_in_ppoll(
            waiter_tid: &AtomicI32,
            waiter_pthread: &AtomicUsize,
            ppoll_nr: i32,
            deadline: std::time::Instant,
        ) -> usize {
            loop {
                let stored_tid = waiter_tid.load(Ordering::Acquire);
                let stored_pthread = waiter_pthread.load(Ordering::Acquire);
                if stored_tid != 0 && stored_pthread != 0 {
                    let path = format!("/proc/{stored_tid}/syscall");
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        let mut parts = text.split_whitespace();
                        if parts.next().and_then(|nr| nr.parse::<i32>().ok()) == Some(ppoll_nr) {
                            return stored_pthread;
                        }
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out acknowledging the exit wait parked in ppoll"
                );
                std::thread::yield_now();
            }
        }

        /// The named bound for the waiter join: poll `is_finished` until the
        /// deadline (yield, never sleep) and only then join, which is
        /// immediate for a finished thread. Returns the waiter's answer and
        /// the instant it captured INSIDE the waiter thread as its blocking
        /// wait returned (ma-sdk-state r18 item 3).
        fn join_waiter_bounded(
            handle: std::thread::JoinHandle<(bool, std::time::Instant)>,
            deadline: std::time::Instant,
            what: &str,
        ) -> (bool, std::time::Instant) {
            while !handle.is_finished() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "bounded join timed out waiting for {what}"
                );
                std::thread::yield_now();
            }
            handle.join().expect("the waiter thread returns")
        }

        /// r17 item 3: the test-only SIGUSR1 registration must not survive
        /// the test. The explicit assertions below cover the normal return;
        /// this guard covers the unwind path (a failed assertion) so the
        /// process-wide handler is removed either way.
        struct SigIdGuard(Option<signal_hook::SigId>);

        impl Drop for SigIdGuard {
            fn drop(&mut self) {
                if let Some(id) = self.0.take() {
                    let _ = signal_hook::low_level::unregister(id);
                }
            }
        }

        // Install the empty handler BEFORE the waiter exists. process-wide,
        // but SIGUSR1 is only ever sent thread-directed at the waiter below, so
        // no other test observes it; the flag additionally records delivery.
        // r17 item 3 unregisters this SigId before the test returns.
        let caught = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sig_id = signal_hook::flag::register(nix::libc::SIGUSR1, Arc::clone(&caught))
            .expect("register the safe SIGUSR1 handler");
        let mut sig_guard = SigIdGuard(Some(sig_id));

        let mut live = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn");
        let live_pid = i32::try_from(live.id()).expect("pid");

        // Probe for the REAL waiter (r17 item 2): where it computes its one
        // deadline, and how many EINTR restarts it actually performs.
        let probe = Arc::new(EintrWaitProbe::default());
        let waiter_tid = Arc::new(AtomicI32::new(0));
        let waiter_pthread = Arc::new(AtomicUsize::new(0));
        let (waiter_tid_for_ack, waiter_pthread_for_signal) =
            (Arc::clone(&waiter_tid), Arc::clone(&waiter_pthread));
        let waiter = std::thread::spawn({
            let probe = Arc::clone(&probe);
            move || {
                waiter_pthread_for_signal.store(pthread_self() as usize, Ordering::Release);
                waiter_tid_for_ack.store(nix::unistd::gettid().as_raw(), Ordering::Release);

                let satisfied = wait_child_exit_blocking(live_pid, WAIT, Some(&probe));
                // ma-sdk-state r18 item 3: stamp the completion instant HERE,
                // on the waiter thread, the moment the blocking wait returns
                // — before the bounded join hands it back to the test thread.
                (satisfied, std::time::Instant::now())
            }
        });

        // ACKNOWLEDGE the wait is ACTIVELY parked in ppoll (no sleep) through
        // the named bounded helper.
        let ack_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let pthread = acknowledge_waiter_parked_in_ppoll(
            &waiter_tid,
            &waiter_pthread,
            PPOLL_NR,
            ack_deadline,
        );

        // Interrupt the parked wait ONCE, directed at the waiter thread.
        pthread_kill(pthread as Pthread, Some(Signal::SIGUSR1))
            .expect("interrupt the waiting thread");

        let (answered, completed) = join_waiter_bounded(
            waiter,
            std::time::Instant::now() + Duration::from_secs(5),
            "the EINTR-interrupted exit waiter",
        );
        assert!(
            caught.load(Ordering::Acquire),
            "the signal reached the caught handler"
        );
        assert!(
            probe.eintr_retries() >= 1,
            "the REAL waiter performed at least one EINTR retry against its deadline"
        );
        assert!(
            !answered,
            "a live child still does not satisfy the exit wait"
        );
        // ma-sdk-state r18 item 3: ONE started instant — sampled by the
        // waiter, stamped unchanged into the probe, and used to derive the
        // deadline — paired with the completion instant captured INSIDE the
        // waiter thread at the blocking return and returned through the
        // bounded join. A live-child wait must satisfy
        // `completion - started >= WAIT`: the old assertion read
        // `started.elapsed()` from the test thread AFTER the join (a second
        // clock sample that tolerated a return before the deadline) and even
        // accepted a return 150 ms early.
        let started = probe
            .deadline_anchor()
            .expect("the real waiter recorded its single started instant");
        let waited = completed
            .checked_duration_since(started)
            .expect("the captured completion instant follows the started instant");
        assert!(
            waited >= WAIT,
            "EINTR must not fail the exit wait early: it continues until the same deadline ({waited:?})"
        );
        let _ = live.kill();
        let _ = live.wait();

        // r17 item 3: the process-wide SIGUSR1 handler is removed BEFORE the
        // normal return. `unregister` reports whether the SigId was still
        // registered (true here), and a second removal finds nothing — the
        // observable proof that the handler does not leak into the rest of
        // the binary.
        let id = sig_guard.0.take().expect("the guard still owns the SigId");
        assert!(
            signal_hook::low_level::unregister(id),
            "the test unregisters its SIGUSR1 SigId before returning"
        );
        assert!(
            !signal_hook::low_level::unregister(id),
            "the removed SIGUSR1 SigId is not registered a second time"
        );
    }
}

/// ma-sdk-state r14 item 4: the child-disposition classifier distinguishes a
/// missing child (ENOENT / ESRCH) from UNAVAILABLE information (unreadable
/// `/proc` for another reason), and reads zombie/alive state correctly.
#[cfg(all(test, target_os = "linux"))]
mod child_disposition_tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    fn ioerr(kind: ErrorKind) -> std::io::Result<String> {
        Err(Error::new(kind, "synthetic /proc failure"))
    }

    #[test]
    fn an_unreadable_proc_is_unknown_when_the_process_exists_not_gone() {
        // The r13 bug: ANY /proc read error mapped to "gone". With the process
        // answering signal 0, EACCES/other is unavailable information.
        assert_eq!(
            classify_linux_child(ioerr(ErrorKind::PermissionDenied), Ok(())),
            ChildDisposition::Unknown
        );
        assert_eq!(
            classify_linux_child(
                ioerr(ErrorKind::PermissionDenied),
                Err(nix::errno::Errno::EPERM)
            ),
            ChildDisposition::Unknown
        );
    }

    #[test]
    fn a_missing_proc_entry_is_gone_only_with_a_confirming_esrch() {
        assert_eq!(
            classify_linux_child(ioerr(ErrorKind::NotFound), Err(nix::errno::Errno::ESRCH)),
            ChildDisposition::Gone
        );
    }

    #[test]
    fn a_hidden_proc_notfound_while_signal0_succeeds_is_unknown() {
        // r15 item 3: inside a chroot or mount namespace /proc/<pid> is
        // ENOENT while the child is alive. ENOENT is a /proc read error like
        // any other — existence (Ok) or EPERM must read Unknown, never Gone.
        assert_eq!(
            classify_linux_child(ioerr(ErrorKind::NotFound), Ok(())),
            ChildDisposition::Unknown,
            "ENOENT + a live signal-0 answer: /proc is hidden, not the child gone"
        );
        assert_eq!(
            classify_linux_child(ioerr(ErrorKind::NotFound), Err(nix::errno::Errno::EPERM)),
            ChildDisposition::Unknown
        );
    }

    #[test]
    fn an_unreadable_proc_with_esrch_is_gone() {
        assert_eq!(
            classify_linux_child(
                ioerr(ErrorKind::PermissionDenied),
                Err(nix::errno::Errno::ESRCH)
            ),
            ChildDisposition::Gone
        );
    }

    #[test]
    fn proc_states_are_classified_zombie_alive_or_unknown() {
        assert_eq!(
            classify_linux_child(Ok("1234 (fake claude) Z 100 1234".into()), Ok(())),
            ChildDisposition::Zombie
        );
        assert_eq!(
            classify_linux_child(Ok("1234 (we)ird (name)) x 1 2".into()), Ok(())),
            ChildDisposition::Zombie,
            "state is parsed AFTER the last ')' so comms with parens work"
        );
        assert_eq!(
            classify_linux_child(Ok("1234 (fake) S 100 1234".into()), Ok(())),
            ChildDisposition::Alive
        );
        assert_eq!(
            classify_linux_child(Ok("garbage".into()), Ok(())),
            ChildDisposition::Unknown,
            "an unparseable stat line is not guessed alive"
        );
    }

    #[test]
    fn a_pid_above_the_kernel_limit_is_gone_or_unknown_never_alive() {
        // 99,999,999 exceeds both Linux PID_MAX_LIMIT and macOS's 99,999 cap.
        let disposition = child_disposition(99_999_999);
        assert_ne!(
            disposition,
            ChildDisposition::Alive,
            "an impossible pid must never look alive"
        );
    }
}

/// ma-sdk-state r15 item 2: the non-Linux unix classifier. These run on EVERY
/// host with injected inputs (the macOS `ps` invocation itself cannot run on
/// the Linux devbox; the coordinator compiles that arm on macOS), which is
/// exactly why the classification is split out of the `ps` call.
#[cfg(all(test, unix))]
mod ps_child_disposition_tests {
    use super::*;

    #[test]
    fn esrch_is_gone_whatever_ps_saw() {
        assert_eq!(
            classify_ps_child(Err(nix::errno::Errno::ESRCH), None),
            ChildDisposition::Gone
        );
        // A stale row must not outrank the kernel's "no such process".
        assert_eq!(
            classify_ps_child(Err(nix::errno::Errno::ESRCH), Some("Ss")),
            ChildDisposition::Gone
        );
    }

    #[test]
    fn a_dead_ps_row_with_signal0_ok_is_a_zombie_not_alive() {
        // The r15 finding: kill(pid,0) succeeds for an unreaped zombie, so the
        // old Ok(()) => Alive arm misclassified SIGKILLed children on macOS.
        for dead in ["Z", "Z+", "E", "Es", "?Es", "?E"] {
            assert_eq!(
                classify_ps_child(Ok(()), Some(dead)),
                ChildDisposition::Zombie,
                "{dead:?} is a dead-but-unreaped process"
            );
        }
    }

    #[test]
    fn a_live_ps_row_with_signal0_ok_is_alive() {
        for live in ["Ss", "Ss+", "?Ss", "R+", "S"] {
            assert_eq!(
                classify_ps_child(Ok(()), Some(live)),
                ChildDisposition::Alive,
                "{live:?} is a running process"
            );
        }
    }

    #[test]
    fn no_ps_row_while_the_pid_exists_is_unknown_never_alive() {
        // `ps` failed or the table did not show the pid: existence without
        // state is unavailable information.
        assert_eq!(classify_ps_child(Ok(()), None), ChildDisposition::Unknown);
    }

    #[test]
    fn eperm_is_unknown_even_with_a_live_looking_row() {
        assert_eq!(
            classify_ps_child(Err(nix::errno::Errno::EPERM), Some("Ss")),
            ChildDisposition::Unknown
        );
        assert_eq!(
            classify_ps_child(Err(nix::errno::Errno::EPERM), None),
            ChildDisposition::Unknown
        );
    }

    #[test]
    fn ps_pid_state_is_parsed_from_ps_o_output() {
        // macOS `ps -o pid=,stat= -p <pid>` shape (leading spaces, state
        // letters with the `?` no-tty marker).
        let out = b"\n  4242 ?Es\n  9999 ?Ss\n";
        assert_eq!(
            parse_ps_pid_state(out, 4242).as_deref(),
            Some("?Es"),
            "the exiting leader's own row"
        );
        assert_eq!(
            parse_ps_pid_state(out, 9999).as_deref(),
            Some("?Ss"),
            "an unrelated row is selectable by pid"
        );
        assert_eq!(
            parse_ps_pid_state(out, 7),
            None,
            "a pid absent from the table yields no row"
        );
    }
}
