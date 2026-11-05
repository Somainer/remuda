//! Containment of a spawned process tree (cb-wake-check r2).
//!
//! A wake-up check is an arbitrary shell command, and `setsid`, a double
//! fork or `nohup … &` take a process out of the shell's process group, so a
//! group kill does not reach it. Two pre-exec steps, both best-effort and both
//! applied in the child after fork and before exec:
//!
//! * [`ContainOnExec::cgroup`]: on Linux, join a cgroup v2 leaf (by writing
//!   `0` to its `cgroup.procs`), so every descendant is in it whatever it
//!   does and one `cgroup.kill` ends them all;
//! * [`ContainOnExec::subreaper`]: on Linux, `prctl(PR_SET_CHILD_SUBREAPER)`.
//!   The attribute survives `execve`, so the exec'd program adopts every
//!   orphan among its descendants (a `setsid` child included) and the caller
//!   can find them all by walking the tree below it.
//!
//! Neither step can fail the spawn: a cgroup that refuses the write, or a
//! kernel without the prctl, leaves the child running uncontained and the
//! caller's own fallback applies.

/// What to arrange in the child before it execs.
#[derive(Debug, Clone, Default)]
pub struct ContainOnExec {
    /// Write `0` to this `cgroup.procs` file (Linux cgroup v2).
    pub cgroup: Option<std::path::PathBuf>,
    /// Become a child subreaper (Linux).
    pub subreaper: bool,
}

impl ContainOnExec {
    /// Install the steps as a pre-exec hook on `command`.
    pub fn install(self, command: &mut std::process::Command) {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt;
            use std::os::unix::process::CommandExt;
            // Everything the hook needs is allocated here, before fork: the
            // hook itself makes only async-signal-safe syscalls.
            let procs = self
                .cgroup
                .and_then(|path| std::ffi::CString::new(path.as_os_str().as_bytes().to_vec()).ok());
            let subreaper = self.subreaper;
            // SAFETY: the hook runs in the forked child before exec. It only
            // calls open/write/close/prctl on memory allocated before the fork
            // and never allocates, locks or unwinds; every failure is ignored.
            unsafe {
                command.pre_exec(move || {
                    if let Some(procs) = &procs {
                        let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                        if fd >= 0 {
                            let _ = libc::write(fd, b"0".as_ptr().cast(), 1);
                            libc::close(fd);
                        }
                    }
                    if subreaper {
                        let _ = libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
                    }
                    Ok(())
                });
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (self, command);
    }
}
