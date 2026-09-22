//! c-hubsupervise: the real `remuda hub` binary under a supervisor.
//!
//! These tests exercise the composition root (CLI flag, signal handler,
//! pid file, parent watch) rather than the in-process `spawn`, because parent
//! death and SIGTERM timing are process-contract properties.
//!
//! The parent-death tests run the Linux path (`prctl(PR_SET_PDEATHSIG)` plus
//! the `/proc` start-time poll). The macOS kqueue (`EVFILT_PROC`/`NOTE_EXIT`)
//! branch is not compiled on this gate; `docs/design/hub-supervise.md` lists
//! the manual Mac verification steps.

#![cfg(unix)]

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::process::{Child, Command};

/// Readiness budget for a loaded gate host (matches the dispatcher e2e).
const READY_BUDGET: Duration = Duration::from_secs(60);
/// Contract: managed SIGTERM shutdown finishes within 3 seconds.
const SIGTERM_BUDGET: Duration = Duration::from_secs(3);
/// Parent-death delivery: prctl is immediate; the poll fallback is 250 ms.
const PARENT_DEATH_BUDGET: Duration = Duration::from_secs(10);

struct Fixture {
    _dir: TempDir,
    data: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let dir = TempDir::new()?;
        Ok(Self {
            data: dir.path().join("data"),
            log: dir.path().join("hub.stderr.log"),
            _dir: dir,
        })
    }

    fn dir(&self) -> &Path {
        self._dir.path()
    }

    fn pid_file(&self) -> PathBuf {
        self.data.join("hub.pid")
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn strip_remuda_env(cmd: &mut Command, cwd: &Path) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("REMUDA_") {
            cmd.env_remove(key);
        }
    }
    cmd.current_dir(cwd)
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
}

/// Spawn `remuda --data-dir D hub --listen 127.0.0.1:0 [--managed p]` directly,
/// so the test process is the hub's real parent.
fn spawn_direct(fx: &Fixture, managed: Option<u32>) -> Result<Child> {
    let log = std::fs::File::create(&fx.log)?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_remuda"));
    strip_remuda_env(&mut cmd, fx.dir());
    let data = fx.data.to_str().context("data dir utf-8")?;
    cmd.args(["--data-dir", data, "hub", "--listen", "127.0.0.1:0"]);
    if let Some(parent_pid) = managed {
        cmd.args(["--managed", &parent_pid.to_string()]);
    }
    cmd.stderr(Stdio::from(log)).kill_on_drop(true);
    Ok(cmd.spawn()?)
}

/// Shell script that starts the hub as a background job and blocks in `wait`,
/// so killing the shell simulates a supervisor crashing while its child hub
/// gets reparented. The hub pid is recorded in `wrapped-hub.pid` for cleanup.
///
/// `managed` adds `--managed $$` (the shell's own pid).
fn spawn_via_shell(fx: &Fixture, managed: bool) -> Result<(Child, PathBuf)> {
    let log = std::fs::File::create(&fx.log)?;
    let child_pid_file = fx.dir().join("wrapped-hub.pid");
    let script = if managed {
        r#""$1" --data-dir "$2" hub --listen 127.0.0.1:0 --managed $$ >/dev/null 2>&1 &
echo $! >"$3"
wait"#
    } else {
        r#""$1" --data-dir "$2" hub --listen 127.0.0.1:0 >/dev/null 2>&1 &
echo $! >"$3"
wait"#
    };
    let mut cmd = Command::new("sh");
    strip_remuda_env(&mut cmd, fx.dir());
    cmd.args([
        "-c",
        script,
        "sh",
        env!("CARGO_BIN_EXE_remuda"),
        fx.data.to_str().context("data dir utf-8")?,
        child_pid_file.to_str().context("pid path utf-8")?,
    ]);
    // The hub inside the shell writes the real log via the shell redirection
    // above; the shell itself stays quiet.
    cmd.stderr(Stdio::from(log)).kill_on_drop(true);
    Ok((cmd.spawn()?, child_pid_file))
}

/// Wait until `<data>/listen` exists and `/healthz` answers, or the process
/// exits first (startup failure).
async fn wait_ready(data: &Path, child: &mut Child) -> Result<SocketAddr> {
    let deadline = Instant::now() + READY_BUDGET;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!(
                "hub exited before becoming ready ({status}); stderr:\n{}",
                std::fs::read_to_string(data.join("..").join("hub.stderr.log")).unwrap_or_default()
            );
        }
        if let Ok(text) = std::fs::read_to_string(data.join("listen"))
            && let Ok(addr) = text.trim().strip_prefix("http://").unwrap_or("").parse()
            && http_get(addr, "/healthz").await.is_ok()
        {
            return Ok(addr);
        }
        if Instant::now() >= deadline {
            bail!("hub did not become ready in {READY_BUDGET:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn http_get(addr: SocketAddr, path: &str) -> Result<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await?;
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .context("malformed HTTP response")?;
    let status = head
        .split_whitespace()
        .nth(1)
        .context("missing status")?
        .parse()
        .context("status code")?;
    Ok((status, body.to_owned()))
}

fn signal(pid: u32, signal: Signal) {
    kill(Pid::from_raw(pid as i32), Some(signal)).expect("signal delivered");
}

fn pid_alive(pid: u32) -> bool {
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

async fn wait_pid_gone(pid: u32, timeout: Duration) -> Result<Duration> {
    let started = Instant::now();
    while pid_alive(pid) {
        if started.elapsed() >= timeout {
            bail!("pid {pid} still alive {timeout:?} after it should have exited");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(started.elapsed())
}

async fn read_file_pid(path: &Path) -> Result<u32> {
    let deadline = Instant::now() + READY_BUDGET;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            return text
                .trim()
                .parse()
                .with_context(|| format!("parse pid file {}", path.display()));
        }
        if Instant::now() >= deadline {
            bail!("pid file {} never appeared", path.display());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// 1. Managed SIGTERM: success exit within 3 seconds, pid file removed.
#[tokio::test]
async fn managed_hub_shuts_down_within_three_seconds_on_sigterm() -> Result<()> {
    let fx = Fixture::new()?;
    let mut child = spawn_direct(&fx, Some(std::process::id()))?;
    let _addr = wait_ready(&fx.data, &mut child).await?;

    let hub_pid = child.id().context("hub pid")?;
    let pid_file = fx.pid_file();
    let recorded = read_file_pid(&pid_file).await?;
    assert_eq!(recorded, hub_pid, "hub.pid must record the hub pid");
    let mode = std::fs::metadata(&pid_file)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "hub.pid must be 0600");

    let started = Instant::now();
    signal(hub_pid, Signal::SIGTERM);
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .context("hub did not exit after SIGTERM")??;
    let elapsed = started.elapsed();
    assert!(
        status.success(),
        "SIGTERM must be a clean (code 0) shutdown: {status}\n{}",
        fx.log_text()
    );
    assert!(
        elapsed < SIGTERM_BUDGET,
        "shutdown took {elapsed:?}, budget is {SIGTERM_BUDGET:?}"
    );
    assert!(!pid_file.exists(), "hub.pid must be removed on clean exit");
    eprintln!("managed SIGTERM shutdown completed in {elapsed:?}");
    Ok(())
}

/// 2. Managed parent killed (SIGKILL — cannot forward anything): the hub must
///    exit by itself and remove its pid file. This is the Linux gate path:
///    prctl(PR_SET_PDEATHSIG) plus the /proc start-time poll fallback.
#[tokio::test]
async fn managed_hub_exits_when_parent_is_killed() -> Result<()> {
    let fx = Fixture::new()?;
    let (mut parent, wrapped_pid_file) = spawn_via_shell(&fx, true)?;
    let _addr = wait_ready(&fx.data, &mut parent).await?;

    let parent_pid = parent.id().context("parent pid")?;
    let hub_pid = read_file_pid(&wrapped_pid_file).await?;
    assert_eq!(read_file_pid(&fx.pid_file()).await?, hub_pid);
    assert!(
        pid_alive(hub_pid),
        "hub should be running before parent death"
    );

    // SIGKILL: the shell cannot run a trap or relay a signal to its child.
    signal(parent_pid, Signal::SIGKILL);
    let _ = tokio::time::timeout(Duration::from_secs(10), parent.wait()).await?;
    let elapsed = wait_pid_gone(hub_pid, PARENT_DEATH_BUDGET).await?;
    assert!(
        !fx.pid_file().exists(),
        "hub.pid must be removed after parent death"
    );
    eprintln!("hub self-exited {elapsed:?} after supervisor SIGKILL");
    Ok(())
}

/// 3. `/healthz` keeps `ok` and gains `version` and `uptimeSecs`.
#[tokio::test]
async fn healthz_reports_version_and_monotonic_uptime() -> Result<()> {
    let fx = Fixture::new()?;
    let mut child = spawn_direct(&fx, Some(std::process::id()))?;
    let addr = wait_ready(&fx.data, &mut child).await?;

    let (status, body) = http_get(addr, "/healthz").await?;
    assert_eq!(status, 200);
    let health: serde_json::Value = serde_json::from_str(&body)?;
    assert_eq!(health["ok"], serde_json::json!(true));
    assert_eq!(
        health["version"],
        serde_json::json!(env!("CARGO_PKG_VERSION")),
        "version must match the remuda package version"
    );
    let first = health["uptimeSecs"]
        .as_u64()
        .context("uptimeSecs must be an unsigned integer")?;
    assert!(first < 60, "fresh hub uptime should be small, got {first}");

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (_, body) = http_get(addr, "/healthz").await?;
    let second = serde_json::from_str::<serde_json::Value>(&body)?["uptimeSecs"]
        .as_u64()
        .context("uptimeSecs must be an unsigned integer")?;
    assert!(second > first, "uptime must advance: {first} -> {second}");

    signal(child.id().context("pid")?, Signal::SIGTERM);
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await?;
    Ok(())
}

/// 4. Without `--managed`: no pid file and no parent-death reaping — the
///    pre-supervision behavior, proven end to end. The hub survives its
///    parent shell being killed and only stops on its own SIGTERM.
#[tokio::test]
async fn unmanaged_hub_runs_unsupervised_like_before() -> Result<()> {
    let fx = Fixture::new()?;
    let (mut parent, wrapped_pid_file) = spawn_via_shell(&fx, false)?;
    let _addr = wait_ready(&fx.data, &mut parent).await?;

    let hub_pid = read_file_pid(&wrapped_pid_file).await?;
    assert!(
        !fx.pid_file().exists(),
        "unmanaged hub must not write hub.pid"
    );

    signal(parent.id().context("parent pid")?, Signal::SIGKILL);
    let _ = tokio::time::timeout(Duration::from_secs(10), parent.wait()).await?;

    // Give any (incorrect) parent-death mechanism longer than its own cadence
    // (the managed poll is 250 ms) to kill the hub, then prove it still serves.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(pid_alive(hub_pid), "unmanaged hub must survive parent exit");
    let listen: SocketAddr = std::fs::read_to_string(fx.data.join("listen"))?
        .trim()
        .strip_prefix("http://")
        .expect("listen url")
        .parse()?;
    let (status, _) = http_get(listen, "/healthz").await?;
    assert_eq!(status, 200, "orphaned-but-unmanaged hub must keep serving");

    // Cleanup: the hub answers its own SIGTERM; reparented, so only liveness
    // is observable, not an exit status.
    signal(hub_pid, Signal::SIGTERM);
    wait_pid_gone(hub_pid, Duration::from_secs(10)).await?;
    assert!(
        !fx.pid_file().exists(),
        "unmanaged hub must never have written hub.pid"
    );
    Ok(())
}
