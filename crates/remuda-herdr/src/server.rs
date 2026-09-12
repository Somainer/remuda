//! Headless `herdr server` lifecycle for isolated sessions.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::client::{Client, default_api_socket, herdr_binary, herdr_config_dir, session_sockets};
use crate::error::Error;

/// A running (or already-running) isolated Herdr headless server.
pub struct HerdrServer {
    session_name: String,
    socket_path: PathBuf,
    child: Option<Child>,
    spawned: bool,
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
        let session_name = session_name.into();
        if session_name.is_empty() {
            return Err(Error::ServerStart {
                session: session_name,
                detail: "session name is empty".into(),
            });
        }

        let (socket_path, use_session_flag) = match socket_dir {
            Some(dir) => (dir.join("herdr.sock"), false),
            None if session_name == "default" => (default_api_socket(), false),
            None => (session_sockets(&session_name).api, true),
        };

        if session_name != "default" && socket_path == default_api_socket() {
            return Err(Error::DefaultSessionGuard {
                socket: socket_path,
            });
        }

        if ping_ok(&socket_path).await {
            info!(session = %session_name, socket = %socket_path.display(), "herdr server already running");
            return Ok(Self {
                session_name,
                socket_path,
                child: None,
                spawned: false,
            });
        }

        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let binary = herdr_binary();
        let mut command = Command::new(&binary);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false)
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_PANE_ID");
        // Never inherit a parent agent's socket.
        command.env_remove("HERDR_SOCKET_PATH");
        if use_session_flag {
            command.arg("--session").arg(&session_name);
            command.env("HERDR_SESSION", &session_name);
        } else {
            command.env("HERDR_SOCKET_PATH", &socket_path);
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
        };
        server.wait_until_ready().await?;
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

async fn ping_ok(socket: &Path) -> bool {
    if !socket.exists() {
        return false;
    }
    let client = Client::connect(socket).with_timeout(Duration::from_secs(2));
    client.ping().await.is_ok()
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
