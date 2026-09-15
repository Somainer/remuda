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
}
