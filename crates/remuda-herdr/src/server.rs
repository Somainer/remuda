//! Headless `herdr server` lifecycle for isolated sessions.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::client::{Client, default_api_socket, herdr_binary, herdr_config_dir, session_sockets};
use crate::error::Error;
use crate::retry::{RetryPolicy, WaitOutcome};

/// How to find, wait for, and start one isolated session server.
#[derive(Debug, Clone)]
pub struct EnsureOptions {
    /// `herdr --session` name. `"default"` opts into the user default socket.
    pub session_name: String,
    /// When set, the API socket is `{socket_dir}/herdr.sock` and the process
    /// is started with `HERDR_SOCKET_PATH` instead of `--session`.
    pub socket_dir: Option<PathBuf>,
    /// Budget for waiting out a predecessor that is still shutting down.
    pub policy: RetryPolicy,
    /// `herdr` executable. Defaults to `$HERDR_BINARY`, else `herdr`.
    pub binary: Option<PathBuf>,
    /// When this handle spawned the server, kill it (SIGTERM, then SIGKILL)
    /// and reap it when the handle is dropped.
    ///
    /// Production leaves this off: a session server is meant to outlive the
    /// transient handle that ensured it. Test harnesses turn it on so a
    /// failed/aborted test cannot leak a server process. The spawned binary
    /// also exits on its own when the parent dies, but tests must not rely on
    /// that path for ordinary cleanup.
    pub kill_on_drop: bool,
}

impl EnsureOptions {
    /// Defaults for a named session: default retry budget, resolved binary.
    #[must_use]
    pub fn new(session_name: impl Into<String>, socket_dir: Option<PathBuf>) -> Self {
        Self {
            session_name: session_name.into(),
            socket_dir,
            policy: RetryPolicy::default(),
            binary: None,
            kill_on_drop: false,
        }
    }

    /// Override the wait budget.
    #[must_use]
    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Override the executable (tests point this at `fake-herdr`).
    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = Some(binary.into());
        self
    }

    /// Kill a server this handle spawned when the handle is dropped.
    #[must_use]
    pub fn with_kill_on_drop(mut self, kill_on_drop: bool) -> Self {
        self.kill_on_drop = kill_on_drop;
        self
    }

    fn resolved_binary(&self) -> PathBuf {
        self.binary.clone().unwrap_or_else(herdr_binary)
    }
}

/// A running (or already-running) isolated Herdr headless server.
pub struct HerdrServer {
    session_name: String,
    socket_path: PathBuf,
    child: Option<Child>,
    spawned: bool,
    /// When true, a spawned child is SIGTERM/SIGKILL'd and reaped on drop.
    kill_on_drop: bool,
    /// Set when [`Self::ensure`] gave up waiting for a predecessor and started
    /// under a suffixed session name instead.
    renamed_from: Option<String>,
}

impl HerdrServer {
    /// Ensure a named session's API socket exists, spawning `herdr server` if needed.
    ///
    /// * `session_name` — `herdr --session` name. The user default session
    ///   (`~/.config/herdr/herdr.sock`) is used only when this is `"default"`.
    /// * `socket_dir` — if `Some`, the API socket is `{socket_dir}/herdr.sock`
    ///   and the process is started with `HERDR_SOCKET_PATH` (no `--session`,
    ///   so Herdr does not rewrite the path). If `None`, the socket is
    ///   `~/.config/herdr/sessions/{name}/herdr.sock`.
    pub async fn ensure(
        session_name: impl Into<String>,
        socket_dir: Option<PathBuf>,
    ) -> Result<Self, Error> {
        Self::ensure_with_policy(session_name, socket_dir, RetryPolicy::default()).await
    }

    /// [`Self::ensure`] with an explicit wait budget for a shutting-down
    /// predecessor.
    ///
    /// A Herdr session name is derived from the Node data dir, so a restarted
    /// Node races the previous server's shutdown on the *same* socket. That
    /// server answers `ping` while refusing real work with
    /// `server_unavailable`, and spawning a second server on a live socket is
    /// refused outright (`herdr server is already running`). So this waits for
    /// the predecessor to exit, then starts fresh; if it never exits within
    /// the budget, it starts under a suffixed session name and warns rather
    /// than failing the caller.
    pub async fn ensure_with_policy(
        session_name: impl Into<String>,
        socket_dir: Option<PathBuf>,
        policy: RetryPolicy,
    ) -> Result<Self, Error> {
        Self::ensure_with(EnsureOptions::new(session_name, socket_dir).with_policy(policy)).await
    }

    /// [`Self::ensure`] with every knob explicit.
    pub async fn ensure_with(options: EnsureOptions) -> Result<Self, Error> {
        let policy = options.policy;
        let socket_dir = options.socket_dir.clone();
        let session_name = options.session_name.clone();
        if session_name.is_empty() {
            return Err(Error::ServerStart {
                session: session_name,
                detail: "session name is empty".into(),
            });
        }

        let (socket_path, use_session_flag) = match socket_dir.clone() {
            Some(dir) => (dir.join("herdr.sock"), false),
            None if session_name == "default" => (default_api_socket(), false),
            None => (session_sockets(&session_name).api, true),
        };

        if session_name != "default" && socket_path == default_api_socket() {
            return Err(Error::DefaultSessionGuard {
                socket: socket_path,
            });
        }

        match endpoint_state(&socket_path).await {
            EndpointState::Ready => {
                info!(session = %session_name, socket = %socket_path.display(), "herdr server already running");
                return Ok(Self {
                    session_name,
                    socket_path,
                    child: None,
                    spawned: false,
                    kill_on_drop: false,
                    renamed_from: None,
                });
            }
            // A predecessor holds the socket but refuses work. Waiting is the
            // whole point: spawning now would only hit "already running".
            EndpointState::ShuttingDown => {
                warn!(
                    session = %session_name,
                    socket = %socket_path.display(),
                    max_wait_secs = policy.max_wait().as_secs(),
                    "herdr server is shutting down; waiting for it to exit before starting a new one"
                );
                if wait_until_gone(&socket_path, policy).await == WaitOutcome::TimedOut {
                    return Self::spawn_under_fallback_name(&options, &socket_path).await;
                }
            }
            EndpointState::Absent => {}
        }

        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let binary = options.resolved_binary();
        let mut command = Command::new(&binary);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(options.kill_on_drop)
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_PANE_ID");
        // Never inherit a parent agent's socket.
        command.env_remove("HERDR_SOCKET_PATH");
        if use_session_flag {
            command.arg("--session").arg(&session_name);
            command.env("HERDR_SESSION", &session_name);
        } else {
            command.env("HERDR_SOCKET_PATH", &socket_path);
            // Keep named-socket servers off the user XDG herdr dir (logs, sessions).
            if let Some(parent) = socket_path.parent() {
                command.env("XDG_CONFIG_HOME", parent);
            }
            if session_name != "default" {
                command.env("HERDR_SESSION", &session_name);
            }
        }
        command.arg("server");

        info!(
            session = %session_name,
            socket = %socket_path.display(),
            binary = %binary.display(),
            "starting herdr server"
        );

        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::BinaryNotFound);
            }
            Err(err) => return Err(err.into()),
        };

        let mut server = Self {
            session_name: session_name.clone(),
            socket_path: socket_path.clone(),
            child: Some(child),
            spawned: true,
            kill_on_drop: options.kill_on_drop,
            renamed_from: None,
        };
        server.wait_until_ready().await?;
        Ok(server)
    }

    /// Last resort: a predecessor outlived the wait budget, so claim a fresh
    /// session name rather than leaving the Node without a carrier.
    ///
    /// The suffix is unique per attempt, so the stuck server keeps its own
    /// socket and its panes are left for manual recovery.
    async fn spawn_under_fallback_name(
        options: &EnsureOptions,
        stuck_socket: &Path,
    ) -> Result<Self, Error> {
        let session_name = options.session_name.clone();
        let suffix = &uuid::Uuid::now_v7().simple().to_string()[..8];
        let fallback = format!("{session_name}-{suffix}");
        // A *sibling* directory, never a child of the stuck server's own dir:
        // that dir belongs to a process still running, and nesting also pushes
        // the socket path past the ~104-byte `sun_path` limit.
        let fallback_dir = options
            .socket_dir
            .as_ref()
            .map(|dir| sibling_dir(dir, suffix))
            .or_else(|| stuck_socket.parent().map(|dir| sibling_dir(dir, suffix)));
        warn!(
            session = %session_name,
            fallback_session = %fallback,
            stuck_socket = %stuck_socket.display(),
            waited_secs = options.policy.max_wait().as_secs(),
            "previous herdr server never exited; starting under a fresh session name. \
             Its panes are left running for manual recovery."
        );
        let mut fallback_options = options.clone();
        fallback_options.session_name = fallback;
        fallback_options.socket_dir = fallback_dir;
        // The fresh name cannot collide with the stuck predecessor, so a second
        // wait would only be dead time.
        fallback_options.policy = RetryPolicy::with_max_wait(Duration::ZERO);
        let mut server = Box::pin(Self::ensure_with(fallback_options)).await?;
        server.renamed_from = Some(session_name);
        Ok(server)
    }

    async fn wait_until_ready(&mut self) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if ping_ok(&self.socket_path).await {
                return Ok(());
            }
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                let stderr = read_child_stderr(child).await;
                return Err(Error::ServerStart {
                    session: self.session_name.clone(),
                    detail: format!("process exited {status}: {stderr}"),
                });
            }
            if Instant::now() >= deadline {
                return Err(Error::ServerStart {
                    session: self.session_name.clone(),
                    detail: format!("timed out waiting for {}", self.socket_path.display()),
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Session name passed to `ensure`.
    #[must_use]
    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    /// API socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Whether this handle spawned the server (vs attaching to an existing one).
    #[must_use]
    pub fn spawned(&self) -> bool {
        self.spawned
    }

    /// The session name originally asked for, when a stuck predecessor forced
    /// a suffixed fallback. `None` on the ordinary path.
    #[must_use]
    pub fn renamed_from(&self) -> Option<&str> {
        self.renamed_from.as_deref()
    }

    /// A client bound to this server.
    #[must_use]
    pub fn client(&self) -> Client {
        Client::connect(&self.socket_path).with_session_name(&self.session_name)
    }

    /// Ask the server to stop via `server.stop`, then `herdr session stop` as fallback.
    pub async fn stop(&mut self) -> Result<(), Error> {
        let client = self.client();
        if let Err(err) = client.call_raw("server.stop", serde_json::json!({})).await {
            warn!(error = %err, "server.stop rpc failed; trying CLI session stop");
            let mut command = tokio::process::Command::new(herdr_binary());
            command
                .arg("--session")
                .arg(&self.session_name)
                .arg("session")
                .arg("stop")
                .env_remove("HERDR_ENV")
                .env_remove("HERDR_PANE_ID")
                .env_remove("HERDR_SOCKET_PATH");
            let _ = command.status().await;
        }
        if let Some(mut child) = self.child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
        Ok(())
    }

    /// `herdr session delete` after stop. No-op for a custom `socket_dir`.
    pub async fn delete_session(&self) -> Result<(), Error> {
        let named = herdr_config_dir().join("sessions").join(&self.session_name);
        if self.socket_path.parent() != Some(named.as_path()) && self.session_name != "default" {
            return Ok(());
        }
        let mut command = tokio::process::Command::new(herdr_binary());
        command
            .arg("session")
            .arg("delete")
            .arg(&self.session_name)
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_SOCKET_PATH");
        let _ = command.status().await;
        Ok(())
    }
}

impl Drop for HerdrServer {
    fn drop(&mut self) {
        if !self.kill_on_drop {
            return;
        }
        if let Some(mut child) = self.child.take() {
            terminate_spawned_child(&mut child);
        }
    }
}

/// SIGTERM a spawned server, escalate to SIGKILL, and reap it — synchronously,
/// so it is safe to call from `Drop`. Bounded waits keep a wedged child from
/// stalling test teardown; `kill_on_drop` reaps anything left over.
fn terminate_spawned_child(child: &mut Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let Some(raw_pid) = child.id().and_then(|id| i32::try_from(id).ok()) else {
            return;
        };
        if raw_pid <= 0 || matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let pid = Pid::from_raw(raw_pid);
        if kill(pid, Signal::SIGTERM).is_ok() {
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        let _ = child.start_kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.try_wait();
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        let _ = child.try_wait();
    }
}

/// `…/herdr` → `…/herdr-<suffix>`, keeping the socket path short enough for
/// `sun_path` (~104 bytes on macOS, 108 on Linux).
fn sibling_dir(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "herdr".to_owned());
    let sibling = format!("{name}-{suffix}");
    match dir.parent() {
        Some(parent) => parent.join(sibling),
        None => PathBuf::from(sibling),
    }
}

async fn ping_ok(socket: &Path) -> bool {
    matches!(endpoint_state(socket).await, EndpointState::Ready)
}

/// What a socket path is currently worth to a caller that wants to do work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointState {
    /// No socket, or nothing listening: safe to spawn.
    Absent,
    /// Answers real requests.
    Ready,
    /// Answers `ping` but refuses work — a predecessor mid-shutdown.
    ShuttingDown,
}

/// Probe with a method that shutdown actually rejects.
///
/// `ping` is answered for the whole shutdown window, so probing with it would
/// report a dying server as healthy — which is exactly the bug: the Node
/// attached, issued `session.snapshot`, and died on `server_unavailable`.
async fn endpoint_state(socket: &Path) -> EndpointState {
    if !socket.exists() {
        return EndpointState::Absent;
    }
    let client = Client::connect(socket).with_timeout(Duration::from_secs(2));
    match client.session_snapshot().await {
        Ok(_) => EndpointState::Ready,
        Err(error) if error.is_server_unavailable() => EndpointState::ShuttingDown,
        // A stale socket file with no listener, or a server too broken to
        // answer, is not something waiting can fix.
        Err(_) => EndpointState::Absent,
    }
}

/// Poll until the predecessor stops refusing work, bounded by `policy`.
///
/// Returns as soon as the endpoint is gone (`Absent`) or usable (`Ready`);
/// either means a spawn can proceed.
async fn wait_until_gone(socket: &Path, policy: RetryPolicy) -> WaitOutcome {
    let started = Instant::now();
    let mut attempt = 0u32;
    loop {
        let Some(backoff) = policy.backoff(attempt, started.elapsed()) else {
            return WaitOutcome::TimedOut;
        };
        tokio::time::sleep(backoff).await;
        attempt = attempt.saturating_add(1);
        match endpoint_state(socket).await {
            EndpointState::ShuttingDown => continue,
            state => {
                info!(
                    socket = %socket.display(),
                    waited_ms = started.elapsed().as_millis(),
                    ?state,
                    "previous herdr server released the socket"
                );
                return WaitOutcome::Ready;
            }
        }
    }
}

async fn read_child_stderr(child: &mut Child) -> String {
    let mut out = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buf).await;
        out.push_str(&String::from_utf8_lossy(&buf));
    }
    if let Some(mut stdout) = child.stdout.take() {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut buf).await;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&String::from_utf8_lossy(&buf));
    }
    out.chars().take(2000).collect()
}
