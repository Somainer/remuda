//! Unix implementation backed by `libc::waitid`.

/// What one non-reaping `waitid` poll established about a child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedChildExit {
    /// The child EXITED; its zombie is deliberately left unreaped (`WNOWAIT`)
    /// so the child's unique owner can signal the rest of its process group
    /// while the unreaped leader still pins the pgid, and reap it afterwards.
    Exited,
    /// No exit event was reported for this child (`WNOHANG`): it is still
    /// running.
    StillRunning,
}

/// Observe whether child `pid` has exited WITHOUT reaping it.
///
/// `waitid(P_PID, pid, WEXITED | WNOHANG | WNOWAIT)`:
/// * `WEXITED` — the required event selector. Without it Linux returns
///   `EINVAL`, which must never be mistaken for an exit;
/// * `WNOHANG` — return immediately with `StillRunning` when nothing exited;
/// * `WNOWAIT` — report the exit but leave the zombie in place for the owner's
///   later `wait`.
///
/// Error discipline (R8(c)): `EINTR` is reported as `StillRunning` (the caller
/// just polls again). Every other error (`EINVAL`, `ECHILD`, …) propagates as
/// `Err` and is explicitly NOT an exit observation: the caller may not treat it
/// as "exited" and may not signal on the strength of it.
///
/// # Safety note
/// The `siginfo_t` is zero-initialised: with `WNOHANG` and no reported event
/// both Linux and macOS leave it untouched, so `si_pid == 0` means "no event";
/// a reported event names the exited child's pid in `si_pid`.
pub fn observe_exit_without_reaping(pid: u32) -> std::io::Result<ObservedChildExit> {
    // SAFETY: `info` is a valid, zero-initialised `siginfo_t` for the call;
    // libc writes only within its layout and we read the safe `si_pid()`
    // accessor after a successful return.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: P_PID with our own child pid, a valid `&mut siginfo_t`, and a
    // fixed flag set that every supported kernel accepts.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            return Ok(ObservedChildExit::StillRunning);
        }
        return Err(error);
    }
    // SAFETY: `info` was passed to a successful waitid; `si_pid()` is a plain
    // field accessor on a value that outlives the call.
    if unsafe { info.si_pid() } == 0 {
        Ok(ObservedChildExit::StillRunning)
    } else {
        Ok(ObservedChildExit::Exited)
    }
}
