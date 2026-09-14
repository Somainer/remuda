//! Make test-only helper binaries exit when the process that spawned them dies.
//!
//! An aborted gate, ctrl-c, or a test timeout kills the spawner with SIGKILL,
//! which gives a child no chance to clean up. Without explicit parent-death
//! handling the helper is reparented to init and runs forever — and the fake
//! herdr server used to busy-loop as well, pinning a core each.
//!
//! Three independent mechanisms are armed together, so the loss of any one
//! (a container without `PR_SET_PDEATHSIG`, a stdin that is `/dev/null`, a
//! subreaper that reparents to a pid other than 1) is covered by another:
//!
//! 1. **`prctl(PR_SET_PDEATHSIG, SIGTERM)`** on Linux: the kernel delivers the
//!    signal the instant the parent dies.
//! 2. **Control-fd hangup**: stdin handed over as a pipe or PTY gets EOF /
//!    `POLLHUP` when the spawner exits. `/dev/null` (also a character device,
//!    but not a tty) and regular files are ignored.
//! 3. **Parent-pid poll** every 250 ms on Linux and macOS: the child exits
//!    when `getppid()` changes, including reparenting to a subreaper.
//!
//! The watcher thread writes a byte to an internal self-pipe (so callers can
//! wait with `poll(2)` or a tokio `AsyncFd`) and hard-exits the process one
//! second later, in case the main loop is wedged and never observes it.

#![cfg_attr(not(unix), allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Delay between parent-pid / stdin-hangup checks.
const WATCH_INTERVAL: Duration = Duration::from_millis(250);

/// Grace period between signalling the self-pipe and hard-exiting.
const HARD_EXIT_GRACE: Duration = Duration::from_secs(1);

/// Armed parent-exit watcher. The read end of the internal notification pipe
/// becomes readable once the parent is gone.
pub(crate) struct ParentWatch {
    /// Read end of the notification pipe; held open for process lifetime.
    #[cfg(unix)]
    notify_fd: std::os::fd::OwnedFd,
}

/// Arm every parent-death mechanism. Returns `None` on platforms without
/// support (the helper simply keeps running there).
pub(crate) fn install() -> Option<ParentWatch> {
    install_with_flag(None)
}

/// Arm every parent-death mechanism and additionally set `flag` once the
/// parent is gone, so a synchronous main loop can shut down gracefully before
/// the watcher's hard-exit deadline.
pub(crate) fn install_with_flag(flag: Option<Arc<AtomicBool>>) -> Option<ParentWatch> {
    #[cfg(unix)]
    {
        Some(ParentWatch::new(flag))
    }
    #[cfg(not(unix))]
    {
        let _ = flag;
        None
    }
}

#[cfg(unix)]
impl ParentWatch {
    fn new(stopping: Option<Arc<AtomicBool>>) -> Self {
        // Ask the kernel to SIGTERM us if the parent dies. Arm before spawning
        // the watcher, then let the watcher re-check getppid(): the parent may
        // already be gone in the gap between fork(2) and this call.
        #[cfg(target_os = "linux")]
        request_parent_death_signal();

        let (read_end, write_end) = notification_pipe().expect("parent-watch notification pipe");

        std::thread::Builder::new()
            .name("parent-watch".into())
            .spawn(move || watch(write_end, stopping))
            .expect("spawn parent-watch thread");

        Self {
            notify_fd: read_end,
        }
    }

    /// Future that completes once the parent is gone.
    pub(crate) async fn exited(&self) {
        use std::os::fd::AsFd;
        let async_fd = tokio::io::unix::AsyncFd::with_interest(
            self.notify_fd.as_fd(),
            tokio::io::Interest::READABLE,
        )
        .expect("register parent-watch fd");
        let _ = async_fd.readable().await;
    }
}

#[cfg(not(unix))]
impl ParentWatch {}

/// Keep Linux's atomic flags; macOS needs `pipe` followed by `fcntl` instead.
/// Both ends are configured before the watcher or any hook child is spawned.
#[cfg(unix)]
fn notification_pipe() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    #[cfg(target_os = "linux")]
    {
        use nix::fcntl::OFlag;
        nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
        use std::os::fd::AsRawFd;

        let ends = nix::unistd::pipe()?;
        for end in [&ends.0, &ends.1] {
            let fd = end.as_raw_fd();
            let flags = FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD)?);
            fcntl(fd, FcntlArg::F_SETFD(flags | FdFlag::FD_CLOEXEC))?;
            let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
            fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        }
        Ok(ends)
    }
}

/// Watcher thread body: poll the parent pid and stdin hangup, then notify the
/// main loop and hard-exit after a grace period.
#[cfg(unix)]
fn watch(notify_write_fd: std::os::fd::OwnedFd, stopping: Option<Arc<AtomicBool>>) {
    use nix::poll::{PollFd, PollFlags, PollTimeout};
    use nix::unistd::getppid;

    let original_parent = getppid();
    // stdin is only meaningful as a parent-liveness channel when the spawner
    // handed us a pipe or a terminal. /dev/null and regular files stay
    // "readable" forever and must not be treated as a hangup.
    let stdin = std::io::stdin();
    use std::os::fd::AsFd;
    let watch_stdin = stdin_is_liveness_channel(stdin.as_fd());
    let poll_timeout = PollTimeout::try_from(WATCH_INTERVAL).expect("250 ms fits PollTimeout");

    loop {
        if getppid() != original_parent {
            break;
        }
        if watch_stdin {
            // POLLHUP/POLLERR are reported regardless of the interest mask;
            // poll also paces the pid re-check at 250 ms.
            let mut pollfd = PollFd::new(stdin.as_fd(), PollFlags::POLLHUP | PollFlags::POLLERR);
            match nix::poll::poll(std::slice::from_mut(&mut pollfd), poll_timeout) {
                Ok(0) => {}
                Ok(_) => {
                    if let Some(revents) = pollfd.revents()
                        && revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR)
                    {
                        break;
                    }
                }
                Err(nix::errno::Errno::EINTR) => {}
                // If stdin stops being pollable, fall back to pid polling.
                Err(_) => std::thread::sleep(WATCH_INTERVAL),
            }
        } else {
            std::thread::sleep(WATCH_INTERVAL);
        }
    }

    notify(&notify_write_fd);
    if let Some(flag) = &stopping {
        flag.store(true, Ordering::SeqCst);
    }
    std::thread::sleep(HARD_EXIT_GRACE);
    // `nix` 0.28 exposes no `_exit` wrapper and the workspace forbids `unsafe`
    // (so libc is off-limits). `process::exit` skips Rust destructors, which is
    // what a wedged main thread needs; the helpers register no atexit work
    // that could deadlock here.
    std::process::exit(0);
}

/// Whether stdin should be watched for spawner exit: a FIFO (pipe whose write
/// end closes on spawner exit) or a real tty (PTY slave that hangs up when its
/// master closes).
#[cfg(unix)]
fn stdin_is_liveness_channel(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    use nix::sys::stat::{SFlag, fstat};
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    let Ok(stat) = fstat(raw) else {
        return false;
    };
    let mode = SFlag::from_bits_truncate(stat.st_mode);
    mode.contains(SFlag::S_IFIFO) || nix::unistd::isatty(raw).unwrap_or(false)
}

/// Wake anything selecting on the notification pipe.
#[cfg(unix)]
fn notify(fd: &std::os::fd::OwnedFd) {
    use nix::unistd::write;
    // Best effort: the pipe is non-blocking and a single byte is enough.
    let _ = write(fd, b"x");
}

/// Request `SIGTERM` when the parent dies (Linux only).
#[cfg(target_os = "linux")]
fn request_parent_death_signal() {
    use nix::sys::prctl::set_pdeathsig;
    use nix::sys::signal::Signal;
    // Default SIGTERM disposition terminates the process; the fake binaries
    // install no SIGTERM handler (fake-harness treats it as a clean stop).
    let _ = set_pdeathsig(Signal::SIGTERM);
}

/// Ask a child to stop, then force-kill and reap it if it does not exit.
///
/// SIGTERM first so a well-behaved helper can unlink sockets and flush
/// artifacts; SIGKILL after `grace` bounds the wait. Tests must not rely on the
/// OS or process-group teardown to reap their helpers.
pub fn terminate_child(child: &mut std::process::Child, grace: Duration) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let raw_pid = child.id() as i32;
        if raw_pid <= 0 || matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let pid = Pid::from_raw(raw_pid);
        if kill(pid, Signal::SIGTERM).is_ok() {
            let deadline = std::time::Instant::now() + grace;
            while std::time::Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        let _ = child.kill();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // Last resort: block until reaped.
        let _ = child.wait();
    }
    #[cfg(not(unix))]
    {
        let _ = child.try_wait();
        let _ = child.kill();
        let _ = child.wait();
        let _ = grace;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
    use std::os::fd::AsRawFd;

    #[tokio::test]
    async fn notification_pipe_has_required_flags_and_wakes_the_waiter() {
        let (read_end, write_end) = notification_pipe().unwrap();
        for end in [&read_end, &write_end] {
            let fd = end.as_raw_fd();
            assert!(
                FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD).unwrap())
                    .contains(FdFlag::FD_CLOEXEC)
            );
            assert!(
                OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL).unwrap())
                    .contains(OFlag::O_NONBLOCK)
            );
        }
        let mut byte = [0];
        assert_eq!(
            nix::unistd::read(read_end.as_raw_fd(), &mut byte),
            Err(nix::errno::Errno::EAGAIN)
        );
        let watcher = ParentWatch {
            notify_fd: read_end,
        };
        notify(&write_end);
        tokio::time::timeout(Duration::from_secs(1), watcher.exited())
            .await
            .expect("notification must wake the async waiter");
        assert_eq!(
            nix::unistd::read(watcher.notify_fd.as_raw_fd(), &mut byte).unwrap(),
            1
        );
        assert_eq!(byte, *b"x");
    }
}
