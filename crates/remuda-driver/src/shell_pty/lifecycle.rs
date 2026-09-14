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

/// Wait up to `grace` for the group to disappear.
async fn settled(pgid: i32, grace: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        if !group_alive(pgid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
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
/// Never returns an error for "the group would not die" — that is a
/// [`StopOutcome`] with `group_gone: false`, which the caller journals as
/// `stop-incomplete`. An `Err` here means the *signal call itself* failed,
/// which is a bug in the caller's pgid, not a stubborn process.
pub(super) async fn stop_group<F>(pgid: i32, mut close_master: F) -> DriverResult<StopOutcome>
where
    F: FnMut(),
{
    #[cfg(not(unix))]
    {
        let _ = (&mut close_master, pgid);
        return Err(DriverError::CapabilityUnsupported(
            "the stop ladder requires unix process groups".into(),
        ));
    }
    #[cfg(unix)]
    {
        use nix::sys::signal::Signal;

        if pgid <= 0 || !group_alive(pgid) {
            close_master();
            return Ok(StopOutcome::already_gone());
        }

        signal_group(pgid, Signal::SIGINT)?;
        if settled(pgid, GRACE_INT).await {
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
        if settled(pgid, GRACE_HUP).await {
            return Ok(StopOutcome {
                rung: StopRung::Hangup,
                group_gone: true,
                pgid: Some(pgid),
                survivors: Vec::new(),
            });
        }

        signal_group(pgid, Signal::SIGKILL)?;
        let gone = settled(pgid, GRACE_KILL).await;
        Ok(StopOutcome {
            rung: StopRung::Kill,
            group_gone: gone,
            pgid: Some(pgid),
            survivors: if gone { Vec::new() } else { survivors(pgid) },
        })
    }
}

/// Pids still in `pgid`, for a `stop-incomplete` diagnostic.
///
/// Diagnostic-only, so shelling out to `ps` is acceptable here in a way it
/// would not be on the liveness path: an empty vec from a missing `ps` weakens
/// the message but never turns a survivor into a clean exit, because
/// [`group_alive`] already made that call.
fn survivors(pgid: i32) -> Vec<i32> {
    use crate::promote::{ProcessTable, SystemProcessTable};
    SystemProcessTable
        .process_group(pgid)
        .into_iter()
        .map(|row| row.pid)
        .collect()
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
        let dir = tempfile::tempdir().expect("tempdir");
        let ready = dir.path().join("ready");
        let pid = spawn_leader(&stubborn(&ready), &ready).await;

        let outcome = stop_group(pid, || {}).await.expect("ladder runs");
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

        let outcome = stop_group(pid, || {}).await.expect("ladder runs");
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
        let outcome = stop_group(pid, || closed = true)
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

        let outcome = stop_group(pid, || {}).await.expect("ladder runs");
        assert!(outcome.group_gone);
        assert!(
            !group_alive(grandchild),
            "the grandchild must not be orphaned; that is the defect §5.3 names"
        );
    }
}
