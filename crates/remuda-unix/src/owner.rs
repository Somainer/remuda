//! THE OWNER CONTRACT (r21; rulings R6/R7/R8) — the single owner helper used
//! by the Node's async land path ([`clean_surviving_group_until`]) and by EVERY
//! recovery git subprocess in both the Node (`remuda-node/src/gate.rs`) and the
//! merge CLI (`remuda/src/cmd/merge.rs`) through [`run_owned_process`].
//!
//! One process group has exactly ONE owner: the calling thread/task. The
//! contract, in order, all of it bounded by ONE absolute deadline:
//!
//! 1. **Concurrent drains.** Reader threads drain stdout AND stderr for the
//!    whole life of the child, CONCURRENTLY with step 2. Readers never reap.
//! 2. **Observe, do not reap.** The leader's exit is observed WITHOUT reaping
//!    (`waitid(WEXITED|WNOHANG|WNOWAIT)`, see
//!    [`crate::observe_exit_without_reaping`]) until the per-command SOFT cap.
//!    An observation error is never an exit. A leader still running at the cap
//!    is SIGKILLed whole-group while unreaped; a normally exited leader gets
//!    step 3.
//! 3. **Clean while unreaped.** Surviving same-group members are signalled
//!    (TERM, bounded settle, unconditional KILL) WHILE the unreaped zombie
//!    leader still pins the pgid.
//! 4. **Reap once.** The leader is reaped exactly once, bounded by the
//!    ABSOLUTE deadline. A reap not accepted by the deadline moves to a
//!    detached owner-side reaper and never blocks the response.
//! 5. **Join both drains inside the deadline.** The two readers are joined
//!    with the remaining budget. A reader still blocked at the deadline is
//!    ABANDONED (dropping its handle detaches the thread); a failed or
//!    incomplete drain of EITHER pipe is uncertainty — never a definitive
//!    result.
//!
//! **Deadline exhaustion is LATCHED.** Once the absolute deadline has expired
//! at ANY step (cleanup, reap, or a drain), the outcome is Unknown/uncertain
//! even if every later step can finish immediately: a completion accepted
//! after expiry does not un-expire the deadline. Reaps still outstanding move
//! to the detached owner; blocked readers are abandoned.

use std::process::{Child, Command, ExitStatus, Output};
use std::time::{Duration, Instant};

/// What an owned subprocess did relative to its absolute deadline.
#[derive(Debug)]
pub enum OwnedRun {
    /// The leader exited before the soft cap AND the owner completed the whole
    /// sequence (survivor kill, leader reap, BOTH pipe drains) before the
    /// absolute deadline. The leader was reaped exactly once.
    Completed(Output),
    /// The leader was still running at the soft cap, the deadline latched
    /// during cleanup/reap, or a pipe drain could not finish by the absolute
    /// deadline. Callers treat this as Unknown.
    TimedOut(Output),
}

impl OwnedRun {
    /// Whether the run should be treated as Unknown.
    pub fn timed_out(&self) -> bool {
        matches!(self, OwnedRun::TimedOut(_))
    }

    /// Whether the leader reaped a successful status (TimedOut is never
    /// success).
    pub fn status_success(&self) -> bool {
        match self {
            OwnedRun::Completed(output) => output.status.success(),
            OwnedRun::TimedOut(_) => false,
        }
    }

    /// Consume the run into its (possibly partial) output.
    pub fn into_output(self) -> Output {
        match self {
            OwnedRun::Completed(output) | OwnedRun::TimedOut(output) => output,
        }
    }
}

/// Owner poll granularity.
const OWNED_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// SIGTERM settle for surviving same-group members before the unconditional
/// KILL (clamped to the absolute deadline).
const OWNED_TERM_GRACE: Duration = Duration::from_millis(750);
/// After the unconditional SIGKILL, how long to wait for the group to clear.
const OWNED_KILL_GRACE: Duration = Duration::from_millis(500);

/// Remaining time until an absolute deadline (saturates at zero).
fn remaining(deadline: Instant) -> Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO)
}

/// Drain one child pipe to EOF on a reader thread that never reaps the leader
/// (the owner is the sole reaper).
///
/// r22 item 3: drains RAW BYTES and reports the outcome explicitly — a read
/// error is `Err`, never an empty/partial success, so the owner can fail
/// closed (Unknown) instead of building a definitive result on a truncated
/// drain. Decoding/interpretation is the caller's job and is lossy.
fn drain_pipe<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        match pipe {
            Some(mut pipe) => pipe.read_to_end(&mut buf).map(|_| buf),
            None => Ok(buf),
        }
    })
}

/// Join a draining reader ONLY while the ABSOLUTE deadline has time left.
///
/// Returns:
/// * `Some(Ok(bytes))` — the reader reached EOF before the deadline,
/// * `Some(Err(_))` — it joined in time but reported a RAW read error, or the
///   join itself failed (a panicked reader): a FAILED drain, which the owner
///   treats as uncertainty (r22 item 3),
/// * `None` — the deadline elapsed first; the thread is ABANDONED (dropping
///   the `JoinHandle` detaches it) and any bytes it eventually produces are
///   not accepted.
///
/// r21 item 3: the deadline is checked BEFORE completion — a reader that
/// finishes AFTER the deadline expired (while the owner was still inside an
/// earlier cleanup/reap step) must not be accepted as an in-time drain.
fn join_drained_until(
    handle: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    deadline: std::time::Instant,
) -> Option<std::io::Result<Vec<u8>>> {
    loop {
        if std::time::Instant::now() >= deadline {
            drop(handle);
            return None;
        }
        if handle.is_finished() {
            // A panicked reader is a FAILED drain, not empty successful bytes.
            return Some(handle.join().unwrap_or_else(|_| {
                Err(std::io::Error::other(
                    "owned-process pipe reader thread panicked before EOF",
                ))
            }));
        }
        std::thread::sleep(OWNED_POLL_INTERVAL.min(remaining(deadline)));
    }
}

/// Linux: whether a non-zombie member OTHER than the unreaped leader is still
/// in `pgid` (`/proc`, timing only — never a death proof). Other platforms
/// cannot enumerate, so they answer "no other members": TERM is sent
/// regardless and the unconditional KILL still reaches every group member.
#[cfg(target_os = "linux")]
fn other_members_alive(leader_pgid: i32) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if pid == leader_pgid {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        let state = fields.first().copied().unwrap_or("");
        let pgrp = fields.get(2).and_then(|v| v.parse::<i32>().ok());
        if pgrp == Some(leader_pgid) && state != "Z" {
            return true;
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn other_members_alive(_leader_pgid: i32) -> bool {
    false
}

/// Send `signal` to the whole process group whose leader is `leader_pgid`
/// (`kill(-pgid, sig)`).
fn kill_group(leader_pgid: i32, signal: libc::c_int) {
    // SAFETY: a negative first argument addresses the process group
    // `leader_pgid` (which this owner spawned with `process_group(0)` and whose
    // unreaped leader keeps it alive); `signal` is a valid libc signal number.
    // An ESRCH (group already gone) or EPERM is ignored: the unconditional
    // KILL and the bounded member enumeration are what bound completion, not
    // this return.
    let _ = unsafe { libc::kill(-leader_pgid, signal) };
}

/// R8(b) contract step 3 — normal-completion cleanup while the leader is an
/// UNREAPED zombie: TERM the whole group, boundedly wait for OTHER members to
/// disappear, then SIGKILL unconditionally and wait again. Every wait is
/// clamped to the SAME absolute deadline.
pub fn clean_surviving_group_until(leader_pgid: i32, deadline: Instant) {
    let clamp = |grace: Duration| std::cmp::min(Instant::now() + grace, deadline);
    kill_group(leader_pgid, libc::SIGTERM);
    let term_until = clamp(OWNED_TERM_GRACE);
    while Instant::now() < term_until && other_members_alive(leader_pgid) {
        std::thread::sleep(OWNED_POLL_INTERVAL.min(remaining(term_until)));
    }
    // Unconditional KILL: never leave a SIGTERM-ignoring transport descendant
    // free to keep a pipe (or a connection) open.
    kill_group(leader_pgid, libc::SIGKILL);
    let kill_until = clamp(OWNED_KILL_GRACE);
    while Instant::now() < kill_until && other_members_alive(leader_pgid) {
        std::thread::sleep(OWNED_POLL_INTERVAL.min(remaining(kill_until)));
    }
}

/// Reap the leader by polling, never blocking, never accepting a completion at
/// or after the absolute deadline. The exit was observed (or the group
/// KILLed), so the zombie is normally collected immediately.
fn reap_bounded(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(OWNED_POLL_INTERVAL.min(remaining(deadline)));
    }
}

/// Hand an unreaped child to a DETACHED owner thread: the eventual reap stays
/// inside this owner process and never blocks the response.
fn detach_reaper(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Error returned when the absolute deadline latched before the reap could be
/// accepted (callers map this to Unknown).
fn deadline_error() -> std::io::Error {
    std::io::Error::other(
        "owned process group did not finish (cleanup/reap/pipe) before the absolute deadline; \
         outcome unknown",
    )
}

/// R6/R7/R8(b) single-owner subprocess runner implementing THE OWNER CONTRACT
/// at the top of this module. `cap` is the per-command SOFT bound (a
/// still-running leader is whole-group SIGKILLed there); `deadline` is the
/// recovery-wide ABSOLUTE bound and governs every subsequent wait.
///
/// Anything not finished by the absolute deadline returns `TimedOut` (leader
/// reaped but a drain incomplete) or an error (the corpse moves to a detached
/// reaper); callers map both to Unknown.
pub fn run_owned_process(
    mut command: Command,
    cap: Duration,
    deadline: Instant,
) -> std::io::Result<OwnedRun> {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let leader_pid = child.id() as i32;
    // Step 1: readers drain both pipes for the whole sequence.
    let out_handle = drain_pipe(child.stdout.take());
    let err_handle = drain_pipe(child.stderr.take());
    let group_deadline = std::cmp::min(Instant::now() + cap, deadline);

    // Step 2: non-reaping exit observation until the soft cap. Observation
    // errors are NOT exits — keep polling.
    let mut leader_exited = false;
    loop {
        if matches!(
            crate::observe_exit_without_reaping(leader_pid as u32),
            Ok(crate::ObservedChildExit::Exited)
        ) {
            leader_exited = true;
            break;
        }
        if Instant::now() >= group_deadline {
            break;
        }
        std::thread::sleep(OWNED_POLL_INTERVAL.min(remaining(group_deadline)));
    }

    #[cfg(test)]
    test_support::maybe_hold_after_exit_observed();

    if leader_exited {
        // Step 3: kill surviving group members while the unreaped leader pins
        // the pgid.
        clean_surviving_group_until(leader_pid, deadline);
    } else {
        // Still running at the soft cap: SIGKILL the WHOLE group while the
        // leader is unreaped.
        kill_group(leader_pid, libc::SIGKILL);
    }

    // LATCH (r21 item 3): if the absolute deadline has expired at this point,
    // NOTHING that follows may produce a definitive result — even a zombie
    // ready to reap and readers already finished. The corpse goes to the
    // detached owner and the readers are abandoned.
    if Instant::now() >= deadline {
        detach_reaper(child);
        drop(out_handle);
        drop(err_handle);
        return Err(deadline_error());
    }

    // Step 4: reap exactly once, bounded by the absolute deadline.
    let Some(status) = reap_bounded(&mut child, deadline) else {
        detach_reaper(child);
        drop(out_handle);
        drop(err_handle);
        return Err(deadline_error());
    };

    // Latch again: expiry during the reap downgrades even an instantly
    // available drain completion to Unknown.
    if Instant::now() >= deadline {
        drop(out_handle);
        drop(err_handle);
        return Ok(OwnedRun::TimedOut(Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }));
    }

    // Step 5: join both drains with the remaining budget. `Some(Ok)` is a
    // complete EOF drain; `None` is an abandoned (deadline-blocked) reader and
    // `Some(Err)` is a raw read/join failure — the last two are uncertainty
    // (r22 item 3), never a Completed result.
    let stdout = join_drained_until(out_handle, deadline);
    let stderr = join_drained_until(err_handle, deadline);
    let drains_done = matches!(stdout, Some(Ok(_))) && matches!(stderr, Some(Ok(_)));
    let output = Output {
        status,
        stdout: stdout.and_then(|result| result.ok()).unwrap_or_default(),
        stderr: stderr.and_then(|result| result.ok()).unwrap_or_default(),
    };
    if leader_exited && drains_done {
        Ok(OwnedRun::Completed(output))
    } else {
        // A blocked reader was abandoned, or the leader had to be KILLed at
        // the cap: the caller maps this to Unknown.
        Ok(OwnedRun::TimedOut(output))
    }
}

/// Platform-independent test seam (r21 items 3/7): a deterministic hold
/// BETWEEN the exit observation and survivor cleanup/reap/drains. Tests arm it
/// so the whole remainder starts only AFTER the absolute deadline, exercising
/// the latched-expiry rule without /proc timing, TERM-ignoring descendants, or
/// unsynchronised background shells.
#[cfg(test)]
mod test_support {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex};

    static ARMED: AtomicBool = AtomicBool::new(false);
    static HOLD_LOCK: Mutex<()> = Mutex::new(());
    static HOLD_CONDVAR: Condvar = Condvar::new();

    /// Park the owner after the exit-observation phase until released.
    pub(super) fn maybe_hold_after_exit_observed() {
        if !ARMED.load(Ordering::Acquire) {
            return;
        }
        let mut guard = HOLD_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while ARMED.load(Ordering::Acquire) {
            guard = HOLD_CONDVAR
                .wait(guard)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    /// Arm the post-exit hold (resets the released state).
    pub fn arm_after_exit_hold() {
        ARMED.store(true, Ordering::Release);
    }

    /// Release an owner parked in [`maybe_hold_after_exit_observed`].
    pub fn release_after_exit_hold() {
        ARMED.store(false, Ordering::Release);
        HOLD_CONDVAR.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::{OwnedRun, drain_pipe, join_drained_until, run_owned_process, test_support};
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    /// A child that exits before the soft cap yields Completed with its own
    /// exit status and drained bytes; its leader was reaped exactly once.
    #[test]
    fn a_quick_child_completes() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo hi; exit 0"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let run = run_owned_process(command, Duration::from_secs(5), deadline).unwrap();
        match run {
            OwnedRun::Completed(output) => {
                assert!(output.status.success());
                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A `Read` that yields one chunk of bytes and then fails, modelling a raw
    /// pipe read error (r22 item 3).
    struct PartialFailingReader {
        chunk: Option<Vec<u8>>,
    }

    impl std::io::Read for PartialFailingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(chunk) = self.chunk.take() {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                Ok(n)
            } else {
                Err(std::io::Error::other("synthetic pipe read failure"))
            }
        }
    }

    /// r22 item 3: a RAW read error is an explicit `Err` from the drain, not an
    /// empty/partial successful drain.
    #[test]
    fn a_failing_pipe_reports_an_error_not_an_empty_drain() {
        let reader = PartialFailingReader {
            chunk: Some(b"partial".to_vec()),
        };
        let handle = drain_pipe(Some(reader));
        let joined = handle.join().expect("reader thread panicked");
        assert!(
            joined.is_err(),
            "a failed read must surface as an error, got {joined:?}"
        );
    }

    /// r22 item 3: `join_drained_until` propagates a finished reader's error as
    /// `Some(Err)` — the owner classifies that as an incomplete drain — while a
    /// successful drain is `Some(Ok(bytes))`.
    #[test]
    fn a_finished_failed_drain_is_some_err_and_a_good_drain_is_some_ok() {
        let failing = drain_pipe(Some(PartialFailingReader {
            chunk: Some(b"x".to_vec()),
        }));
        let outcome = join_drained_until(failing, Instant::now() + Duration::from_secs(2));
        match outcome {
            Some(Err(_)) => {}
            other => panic!("a failed drain must be Some(Err), got {other:?}"),
        }

        let good = drain_pipe(Some(std::io::Cursor::new(b"complete".as_slice())));
        let outcome = join_drained_until(good, Instant::now() + Duration::from_secs(2));
        match outcome {
            Some(Ok(bytes)) => assert_eq!(bytes, b"complete"),
            other => panic!("a full EOF drain must be Some(Ok), got {other:?}"),
        }
    }

    /// A still-running leader at the soft cap is SIGKILLed whole-group while
    /// unreaped and the run is TimedOut (signal 9), well inside the absolute
    /// deadline. Portable: the assertion is on the leader's status, not on
    /// process enumeration.
    #[test]
    fn a_leader_past_the_soft_cap_is_killed_and_times_out() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 47.03"]);
        let started = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(5);
        let run = run_owned_process(command, Duration::from_millis(300), deadline).unwrap();
        use std::os::unix::process::ExitStatusExt;
        let output = match run {
            OwnedRun::TimedOut(output) => output,
            other => panic!("expected TimedOut, got {other:?}"),
        };
        assert_eq!(output.status.signal(), Some(9), "{:?}", output.status);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// r20 item 5: the leader EXITS FIRST (nonzero) while a same-group
    /// grandchild survives with inherited stdout/stderr. The owner observes
    /// without reaping, kills the survivor while the unreaped leader pins the
    /// pgid, reaps and returns the LEADER's status as Completed, bounded —
    /// never waiting on the grandchild's sleep. Portable core assertions; the
    /// no-leftover-process check is Linux (/proc) only.
    #[test]
    fn normal_exit_cleans_survivors_and_keeps_the_leader_status() {
        let mut command = Command::new("sh");
        command.args(["-c", "(sleep 47.04) & exit 42"]);
        let started = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(5);
        let run = run_owned_process(command, Duration::from_secs(2), deadline).unwrap();
        let output = match run {
            OwnedRun::Completed(output) => output,
            other => panic!("expected Completed after survivor cleanup, got {other:?}"),
        };
        assert_eq!(output.status.code(), Some(42));
        assert!(started.elapsed() < Duration::from_secs(4));
        #[cfg(target_os = "linux")]
        {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let left = live_marker_processes("sleep 47.04");
                if left == 0 || Instant::now() >= deadline {
                    assert_eq!(left, 0, "the surviving grandchild outlived the owner");
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// r21 items 3/7 — THE LATCH: a child that exits immediately with no pipe
    /// holders is held AFTER the exit observation until the absolute deadline
    /// has expired. Cleanup, the reap and both drains can then finish
    /// INSTANTLY, but completion after expiry must be Unknown: an error (the
    /// corpse moves to the detached reaper), never Completed. Platform
    /// independent — no descendant, no pipe pressure, no /proc.
    #[test]
    fn completion_after_the_deadline_is_latched_unknown() {
        test_support::arm_after_exit_hold();
        let command = Command::new("true");
        let deadline = Instant::now() + Duration::from_millis(300);
        let owner =
            thread::spawn(move || run_owned_process(command, Duration::from_secs(10), deadline));
        // Let the absolute deadline expire while the owner is parked after the
        // (immediate) exit observation.
        thread::sleep(Duration::from_millis(700));
        test_support::release_after_exit_hold();
        let started = Instant::now();
        let result = owner.join().expect("owner thread panicked");
        assert!(
            result.is_err(),
            "a reap/drain sequence completed only after the absolute deadline must be Unknown, \
             got {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the latched owner must return promptly after release"
        );
    }

    /// Count LIVE (non-zombie) processes whose /proc/<pid>/cmdline carries the
    /// marker (Linux assertions only).
    #[cfg(target_os = "linux")]
    fn live_marker_processes(marker: &str) -> usize {
        std::fs::read_dir("/proc")
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                    .filter_map(|name| name.parse::<i32>().ok())
                    .filter(|pid| {
                        let base = std::path::Path::new("/proc").join(pid.to_string());
                        let Ok(cmd) = std::fs::read_to_string(base.join("cmdline")) else {
                            return false;
                        };
                        if !cmd.replace('\0', " ").contains(marker) {
                            return false;
                        }
                        if let Ok(stat) = std::fs::read_to_string(base.join("stat"))
                            && let Some(after) = stat.rsplit_once(')').map(|(_, r)| r)
                        {
                            let state = after.split_whitespace().next().unwrap_or("");
                            return state != "Z";
                        }
                        true
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}
