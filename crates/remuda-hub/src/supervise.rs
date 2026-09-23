//! Managed-process supervision for `remuda hub --managed <parentPid>` (c-hubsupervise).
//!
//! A Hub launched by a tray helper or desktop shell must not outlive the
//! process that supervises it: a crashed supervisor would leave an unreachable
//! Hub holding the data directory. This module provides two independent pieces
//! the composition root wires in managed mode:
//!
//! 1. [`ParentWatch`] — resolves when the supervisor process exits.
//! 2. [`PidFile`] — `<data_dir>/hub.pid`, written on startup and removed on
//!    clean exit.
//!
//! ## Parent-death mechanisms by platform
//!
//! Every unix platform runs the same 250 ms `kill(pid, 0)` poll on the
//! *named* supervisor pid, so the detection path is always compiled and
//! tested by the merge gate (it runs on Linux). A parent that exited is at
//! most one poll interval (~250 ms) of dying undetected — acceptable for a
//! managed Hub.
//!
//! * **Linux additionally arms `prctl(PR_SET_PDEATHSIG, SIGTERM)`** when the
//!   named supervisor is this process's real parent (the normal tray → hub
//!   shape): the kernel then delivers SIGTERM the instant the parent dies, and
//!   the CLI's existing signal handler runs the same graceful shutdown as an
//!   operator SIGTERM. The poll still runs alongside it and independently
//!   covers non-parent pids (a launcher that is not the direct parent),
//!   kernels/containers without the prctl, and subreaper reparenting. The
//!   Linux poll compares the supervisor's start time from
//!   `/proc/<pid>/stat` so pid reuse cannot be mistaken for the supervisor
//!   still being alive.
//! * **macOS and every other non-Linux unix use the poll alone.** A macOS
//!   kqueue (`EVFILT_PROC`/`NOTE_EXIT`) would be marginally quicker, but that
//!   code is `#[cfg]`-invisible to the Linux gate and previously shipped
//!   uncompiled; one tested mechanism is worth more than a fast untested one.
//!
//! The mechanisms never conflict: both merely ask the run loop to begin the
//! same graceful shutdown.
//!
//! Design and rationale: `docs/design/hub-supervise.md`.

#![cfg_attr(not(unix), allow(dead_code))]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// How often the parent-pid poll wakes. Matches the 250 ms cadence already
/// used by the test helper parent watch (`remuda-testing`).
const PARENT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Hub version reported by `GET /healthz`. Workspace versions move together,
/// so the hub crate's package version is also the `remuda` CLI version.
pub const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Process start time, stamped the first time a Hub is spawned in this process.
static STARTED_AT: OnceLock<Instant> = OnceLock::new();

/// Record process start. Called once per process from the Hub spawn path;
/// tests that spawn many in-process Hubs still measure process uptime, which
/// is what `uptimeSecs` promises.
pub(crate) fn mark_started() {
    let _ = STARTED_AT.set(Instant::now());
}

/// Whole seconds since this process started (0 within the first second).
#[must_use]
pub fn uptime_secs() -> u64 {
    STARTED_AT.get_or_init(Instant::now).elapsed().as_secs()
}

/// Future returned by [`watch_parent`]; resolves once the supervisor is gone.
pub struct ParentWatch {
    rx: oneshot::Receiver<()>,
}

impl ParentWatch {
    /// Wait until the supervisor process has exited.
    pub async fn exited(&mut self) {
        // The watcher thread always sends exactly once; an Err means it
        // stopped while the Hub was already shutting down.
        let _ = (&mut self.rx).await;
    }
}

/// Begin watching the supervisor named by `--managed <parentPid>`.
///
/// Fails at startup if the pid is unusable or the supervisor is already gone:
/// a Hub that starts orphaned must exit instead of running unsupervised.
pub fn watch_parent(parent_pid: u32) -> anyhow::Result<ParentWatch> {
    let parent_pid = parse_parent_pid(parent_pid)?;
    anyhow::ensure!(parent_pid > 0, "--managed parent pid must be positive");
    anyhow::ensure!(
        i64::from(parent_pid) != i64::from(std::process::id()),
        "--managed parent pid cannot be the hub process itself"
    );
    spawn_watch(parent_pid)
}

fn parse_parent_pid(parent_pid: u32) -> anyhow::Result<i32> {
    i32::try_from(parent_pid)
        .map_err(|_| anyhow::anyhow!("--managed parent pid out of range: {parent_pid}"))
}

/// `<data_dir>/hub.pid` guard. Removal happens on drop, i.e. after the server
/// has stopped and the journal has been flushed, so a supervisor polling the
/// file only ever sees a pid that owns a running (or starting) Hub.
pub struct PidFile {
    path: PathBuf,
}

impl PidFile {
    /// Write `<data_dir>/hub.pid` containing this process's pid.
    pub fn write(data_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("hub.pid");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
        file.sync_all()?;
        Ok(Self { path })
    }

    /// Path of the PID file, for tests and support tooling.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        // A file left behind by SIGKILL/power loss is stale, not authoritative:
        // the supervisor must verify the pid before trusting it.
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), error = %error, "cannot remove hub.pid");
        }
    }
}

// Platform implementations ---------------------------------------------------

#[cfg(unix)]
fn spawn_watch(parent_pid: i32) -> anyhow::Result<ParentWatch> {
    // Arm before the first liveness check and before spawning the watcher, so
    // the kernel is already watching for the fork-window race (prctl(2): "the
    // thread will not get a signal if the parent ... died before prctl() was
    // called" — the explicit check immediately after closes that window).
    #[cfg(target_os = "linux")]
    arm_pdeathsig(parent_pid);

    let marker = ProcMarker::snapshot(parent_pid);
    anyhow::ensure!(
        marker.is_alive(parent_pid),
        "--managed parent pid {parent_pid} is not running"
    );
    let (tx, rx) = oneshot::channel();
    thread::Builder::new()
        .name("hub-parent-watch".into())
        .spawn(move || marker.watch(parent_pid, tx))
        .map_err(|error| anyhow::anyhow!("spawn parent watch thread: {error}"))?;
    Ok(ParentWatch { rx })
}

#[cfg(not(unix))]
fn spawn_watch(_parent_pid: i32) -> anyhow::Result<ParentWatch> {
    anyhow::bail!("remuda hub --managed is supported on unix only")
}

/// Whether a pid exists via the null signal. `EPERM` means the process exists
/// but belongs to another user, so it counts as alive.
#[cfg(unix)]
fn pid_exists(pid: i32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

// Linux: prctl acceleration + /proc starttime-protected poll ---------------

#[cfg(all(unix, target_os = "linux"))]
struct ProcMarker {
    /// Field 22 (`starttime`, clock ticks since boot) of `/proc/<pid>/stat`.
    /// `None` if it could not be read at arm time; the poll falls back to the
    /// null-signal check alone.
    starttime: Option<u64>,
}

#[cfg(all(unix, target_os = "linux"))]
impl ProcMarker {
    fn snapshot(pid: i32) -> Self {
        Self {
            starttime: read_starttime(pid),
        }
    }

    fn is_alive(&self, pid: i32) -> bool {
        match (pid_exists(pid), self.starttime) {
            (false, _) => false,
            // Process visible, but a reused pid carries a different start time.
            (true, Some(starttime)) => read_starttime(pid).is_none_or(|now| now == starttime),
            (true, None) => true,
        }
    }

    fn watch(self, pid: i32, tx: oneshot::Sender<()>) {
        while self.is_alive(pid) {
            thread::sleep(PARENT_POLL_INTERVAL);
        }
        let _ = tx.send(());
    }
}

#[cfg(all(unix, target_os = "linux"))]
fn read_starttime(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` (field 2) can contain spaces and parentheses; split at the final
    // ')' and tokenize the remainder, where field 3 (`state`) is token 0.
    let mut after_comm = stat.rsplit_once(')')?.1.split_whitespace();
    // starttime is field 22 overall → the 20th token after `)`.
    after_comm.nth(19)?.parse::<u64>().ok()
}

#[cfg(all(unix, target_os = "linux"))]
fn arm_pdeathsig(parent_pid: i32) {
    use nix::sys::prctl::set_pdeathsig;
    use nix::sys::signal::Signal;
    use nix::unistd::getppid;
    // PR_SET_PDEATHSIG tracks the thread that forked this process, not an
    // arbitrary pid. It is only meaningful when the named supervisor is that
    // parent; the marker poll covers every other shape.
    if parent_pid != getppid().as_raw() {
        return;
    }
    if let Err(error) = set_pdeathsig(Some(Signal::SIGTERM)) {
        tracing::warn!(error = %error, "PR_SET_PDEATHSIG unavailable; falling back to parent pid poll");
    }
}

// Non-Linux unix (macOS, …): the cross-platform null-signal poll only.
//
// Deliberately no macOS-only kqueue branch: code behind
// `#[cfg(target_os = "macos")]` is invisible to the Linux merge gate and
// cannot be tested here, so it would ship unverified. The poll's ~250 ms
// detection lag is acceptable for a managed Hub.

#[cfg(all(unix, not(target_os = "linux")))]
struct ProcMarker;

#[cfg(all(unix, not(target_os = "linux")))]
impl ProcMarker {
    fn snapshot(_pid: i32) -> Self {
        Self
    }

    fn is_alive(&self, pid: i32) -> bool {
        pid_exists(pid)
    }

    fn watch(self, pid: i32, tx: oneshot::Sender<()>) {
        while pid_exists(pid) {
            thread::sleep(PARENT_POLL_INTERVAL);
        }
        let _ = tx.send(());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn rejects_unusable_parent_pids() {
        assert!(watch_parent(0).is_err(), "pid 0 is not a process");
        assert!(
            watch_parent(std::process::id()).is_err(),
            "the hub cannot supervise itself"
        );
        // No platform hands out pids anywhere near i32::MAX, so this is
        // deterministically dead without racing pid reuse.
        assert!(
            watch_parent(i32::MAX as u32).is_err(),
            "a dead parent pid must refuse startup"
        );
    }

    #[test]
    fn null_signal_liveness_detects_self_and_misses_unknown_pid() {
        assert!(pid_exists(std::process::id() as i32));
        assert!(!pid_exists(i32::MAX));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_own_proc_starttime() {
        let starttime =
            read_starttime(std::process::id() as i32).expect("/proc/self/stat field 22");
        assert!(starttime > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn marker_rejects_reused_pid() {
        let pid = std::process::id() as i32;
        // A marker carrying a start time from a different process "generation"
        // must read as dead even though kill(pid, 0) succeeds: the pid was
        // reused. This is the whole point of snapshotting field 22.
        let stale = ProcMarker {
            starttime: Some(u64::MAX),
        };
        assert!(!stale.is_alive(pid));
        // No start time (unparseable /proc) degrades to the null-signal check.
        assert!(ProcMarker { starttime: None }.is_alive(pid));
        // A fresh snapshot of a live pid reads alive.
        assert!(ProcMarker::snapshot(pid).is_alive(pid));
        // A certainly-free pid reads dead under every marker shape.
        assert!(!ProcMarker { starttime: Some(1) }.is_alive(i32::MAX));
        assert!(!ProcMarker { starttime: None }.is_alive(i32::MAX));
    }

    #[test]
    fn pid_file_lifecycle_records_this_process_and_cleans_up() {
        let dir =
            std::env::temp_dir().join(format!("hub-supervise-pidfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pid_file = PidFile::write(&dir).expect("write pid file");
        let contents = std::fs::read_to_string(pid_file.path()).expect("read pid file");
        assert_eq!(contents.trim(), std::process::id().to_string());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(pid_file.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let path = pid_file.path().to_path_buf();
        drop(pid_file);
        assert!(!path.exists(), "pid file removed on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uptime_advances() {
        let before = uptime_secs();
        std::thread::sleep(Duration::from_millis(1_100));
        assert!(uptime_secs() > before);
    }
}
