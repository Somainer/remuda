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
    let join = tokio::task::spawn_blocking(move || wait_child_exit_blocking(pid, timeout));
    join.await.unwrap_or_default()
}

/// What one bounded event wait ended on (ma-sdk-state r16 item 2).
#[cfg(all(any(test, feature = "test-stub"), unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptibleEventWait {
    /// The termination event fired.
    Fired,
    /// The single bound elapsed without an event.
    TimedOut,
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
fn wait_with_eintr_retry(
    timeout: Duration,
    mut wait_once: impl FnMut(Duration) -> std::io::Result<InterruptibleEventWait>,
) -> std::io::Result<InterruptibleEventWait> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining == Duration::ZERO {
            return Ok(InterruptibleEventWait::TimedOut);
        }
        match wait_once(remaining) {
            Ok(outcome) => return Ok(outcome),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(any(test, feature = "test-stub"), unix, target_os = "linux"))]
fn wait_child_exit_blocking(pid: i32, timeout: Duration) -> bool {
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
    let waited = wait_with_eintr_retry(timeout, |remaining| {
        let Some(timespec) = Timespec::try_from(remaining).ok() else {
            return Ok(InterruptibleEventWait::TimedOut);
        };
        match poll(&mut fds, Some(&timespec)) {
            Ok(1) => Ok(InterruptibleEventWait::Fired),
            Ok(_) => Ok(InterruptibleEventWait::TimedOut),
            Err(rustix::io::Errno::INTR) => Err(std::io::Error::from(rustix::io::Errno::INTR)),
            Err(other) => Err(std::io::Error::from(other)),
        }
    });
    matches!(waited, Ok(InterruptibleEventWait::Fired))
        && matches!(
            child_disposition(pid),
            ChildDisposition::Gone | ChildDisposition::Zombie
        )
}

#[cfg(all(any(test, feature = "test-stub"), unix, not(target_os = "linux")))]
fn wait_child_exit_blocking(pid: i32, timeout: Duration) -> bool {
    // The macOS/BSD equivalent of the pidfd wait: an EVFILT_PROC/NOTE_EXIT
    // registration delivers when the watched process exits. nix's safe
    // kqueue wrapper is used because the workspace forbids `unsafe`; same-uid
    // processes (our own test children) are watchable though not reaped by us.
    use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};

    let Ok(kq) = Kqueue::new() else {
        return false;
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
    // A delivered NOTE_EXIT and a changelist EV_ERROR (the process was
    // already gone) both return 1; 0 is the timeout. EINTR retries against
    // the ONE monotonic deadline with the remaining time (r16 item 2).
    let waited = wait_with_eintr_retry(timeout, |remaining| {
        let deadline = Some(nix::libc::timespec {
            tv_sec: remaining.as_secs() as nix::libc::time_t,
            tv_nsec: remaining.subsec_nanos() as nix::libc::c_long,
        });
        match kq.kevent(&[change], &mut events, deadline) {
            Ok(1) => Ok(InterruptibleEventWait::Fired),
            Ok(_) => Ok(InterruptibleEventWait::TimedOut),
            Err(nix::errno::Errno::EINTR) => Err(std::io::Error::from(nix::errno::Errno::EINTR)),
            Err(other) => Err(std::io::Error::from(other)),
        }
    });
    matches!(waited, Ok(InterruptibleEventWait::Fired))
        && matches!(
            child_disposition(pid),
            ChildDisposition::Gone | ChildDisposition::Zombie
        )
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
        })
    }
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
        let outcome = wait_with_eintr_retry(TIMEOUT, |remaining| {
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
                _ => Ok(InterruptibleEventWait::Fired),
            }
        })
        .expect("the wait itself never returns an error");
        assert_eq!(outcome, InterruptibleEventWait::Fired);
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
        let outcome = wait_with_eintr_retry(BOUND, |_| {
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

    /// r16 item 2 (injectable): any error other than EINTR propagates on the
    /// first attempt — an unexpected syscall failure still fails honestly.
    #[cfg(unix)]
    #[test]
    fn a_non_eintr_wait_error_propagates_immediately() {
        let error = wait_with_eintr_retry(Duration::from_secs(60), |_| {
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
        let outcome = wait_with_eintr_retry(
            Duration::ZERO,
            |_| -> std::io::Result<InterruptibleEventWait> {
                unreachable!("no wait attempt happens after the deadline is gone")
            },
        )
        .expect("the wait itself never returns an error");
        assert_eq!(outcome, InterruptibleEventWait::TimedOut);
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
        use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

        /// The bound: short enough for a hot EINTR retry loop, long enough to
        /// separate the old fail-immediate behavior from a continued wait.
        const WAIT: Duration = Duration::from_millis(400);
        /// ppoll(2) on x86_64.
        const PPOLL_NR: i32 = 271;

        // Install the empty handler BEFORE the waiter exists. process-wide,
        // but SIGUSR1 is only ever sent thread-directed at the waiter below, so
        // no other test observes it; the flag additionally records delivery.
        let caught = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(nix::libc::SIGUSR1, Arc::clone(&caught))
            .expect("register the safe SIGUSR1 handler");

        let mut live = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn");
        let live_pid = i32::try_from(live.id()).expect("pid");

        let waiter_tid = Arc::new(AtomicI32::new(0));
        let waiter_pthread = Arc::new(AtomicUsize::new(0));
        let (waiter_tid_for_ack, waiter_pthread_for_signal) =
            (Arc::clone(&waiter_tid), Arc::clone(&waiter_pthread));
        let waiter = std::thread::spawn(move || {
            waiter_pthread_for_signal.store(pthread_self() as usize, Ordering::Release);
            waiter_tid_for_ack.store(nix::unistd::gettid().as_raw(), Ordering::Release);

            let started = std::time::Instant::now();
            let answered = wait_child_exit_blocking(live_pid, WAIT);
            (answered, started.elapsed())
        });

        // ACKNOWLEDGE the wait is ACTIVELY parked in ppoll (no sleep): read
        // the waiter's syscall file until it names ppoll, bounded by the named
        // ack deadline below.
        let ack_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (tid, pthread) = loop {
            let stored_tid = waiter_tid.load(Ordering::Acquire);
            let stored_pthread = waiter_pthread.load(Ordering::Acquire);
            if stored_tid != 0 && stored_pthread != 0 {
                let path = format!("/proc/{stored_tid}/syscall");
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let mut parts = text.split_whitespace();
                    if parts.next().and_then(|nr| nr.parse::<i32>().ok()) == Some(PPOLL_NR) {
                        break (stored_tid, stored_pthread);
                    }
                }
            }
            assert!(
                std::time::Instant::now() < ack_deadline,
                "timed out acknowledging the exit wait parked in ppoll"
            );
            std::thread::yield_now();
        };
        let _ = tid;

        // Interrupt the parked wait ONCE, directed at the waiter thread.
        pthread_kill(pthread as Pthread, Some(Signal::SIGUSR1))
            .expect("interrupt the waiting thread");

        let (answered, elapsed) = waiter.join().expect("the waiter thread returns");
        assert!(
            caught.load(Ordering::Acquire),
            "the signal reached the caught handler"
        );
        assert!(
            !answered,
            "a live child still does not satisfy the exit wait"
        );
        assert!(
            elapsed >= WAIT - Duration::from_millis(150),
            "EINTR must not fail the exit wait early: it continues until the same deadline ({elapsed:?})"
        );
        let _ = live.kill();
        let _ = live.wait();
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
