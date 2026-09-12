//! [`NodeTransport`]: SSH stdio (length-prefixed JSON) and WebSocket (one message per JSON).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::client::{SshClient, spawn_ssh};
use crate::error::Error;
use crate::frame::{MAX_JSON_FRAME_BYTES, read_json_frame, write_json_frame};

/// Hub↔Node JSON carrier. Stdio uses a 4-byte length prefix; WSS uses WS message bounds.
pub trait NodeTransport {
    /// Send one JSON value.
    fn send_json(&mut self, value: &Value) -> impl Future<Output = Result<(), Error>> + Send;

    /// Receive one JSON value. `Ok(None)` means a clean close.
    fn recv_json(&mut self) -> impl Future<Output = Result<Option<Value>, Error>> + Send;

    /// Close the carrier.
    fn close(&mut self) -> impl Future<Output = Result<(), Error>> + Send;
}

/// Exponential backoff after an SSH stdio disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay after the first unexpected exit.
    pub initial: Duration,
    /// Cap.
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

impl Backoff {
    /// Delay before `attempt` (0-based) reconnect.
    #[must_use]
    pub fn delay(self, attempt: u32) -> Duration {
        let mut current = self.initial;
        for _ in 0..attempt {
            current = current.saturating_mul(2).min(self.max);
        }
        current
    }
}

#[derive(Debug, Clone)]
enum Recipe {
    Ssh {
        client: SshClient,
        remote_argv: Vec<String>,
    },
    Local {
        program: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
}

/// `ssh <alias> -- <remote_bin> node --stdio` (or a local test double).
pub struct StdioTransport {
    recipe: Recipe,
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// Reconnect policy after ssh exits.
    pub backoff: Backoff,
    /// Length-prefix cap.
    pub max_frame_bytes: u32,
}

impl StdioTransport {
    /// Open `ssh -T <alias> -- <remote_argv…>` with piped stdio.
    pub async fn connect_ssh(client: SshClient, remote_argv: Vec<String>) -> Result<Self, Error> {
        let recipe = Recipe::Ssh {
            client,
            remote_argv,
        };
        Self::spawn(recipe, Backoff::default(), MAX_JSON_FRAME_BYTES).await
    }

    /// Spawn a local program that speaks length-prefixed JSON on stdio (tests).
    pub async fn connect_local(
        program: impl Into<PathBuf>,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Result<Self, Error> {
        let recipe = Recipe::Local {
            program: program.into(),
            args,
            env,
        };
        Self::spawn(recipe, Backoff::default(), MAX_JSON_FRAME_BYTES).await
    }

    async fn spawn(recipe: Recipe, backoff: Backoff, max_frame_bytes: u32) -> Result<Self, Error> {
        let mut child = match &recipe {
            Recipe::Ssh {
                client,
                remote_argv,
            } => {
                let mut cmd = client.command(remote_argv)?;
                cmd.stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                spawn_ssh(&client.options.ssh_binary, cmd)?
            }
            Recipe::Local { program, args, env } => {
                let mut cmd = Command::new(program);
                cmd.args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                for (key, value) in env {
                    cmd.env(key, value);
                }
                cmd.spawn()?
            }
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Io(std::io::Error::other("stdin pipe missing after spawn")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Io(std::io::Error::other("stdout pipe missing after spawn")))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "remuda_ssh::remote_stderr", "{line}");
                }
            });
        }
        Ok(Self {
            recipe,
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            backoff,
            max_frame_bytes,
        })
    }

    /// True when the ssh/local child has exited.
    pub fn child_exited(&mut self) -> Result<bool, Error> {
        Ok(self.child.try_wait()?.is_some())
    }

    /// Reconnect after the child has gone, sleeping per [`Backoff`].
    pub async fn reconnect(&mut self) -> Result<(), Error> {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        let mut attempt = 0_u32;
        loop {
            let delay = self.backoff.delay(attempt);
            tracing::warn!(attempt, ?delay, "stdio transport reconnecting");
            sleep(delay).await;
            match Self::spawn(self.recipe.clone(), self.backoff, self.max_frame_bytes).await {
                Ok(next) => {
                    *self = next;
                    return Ok(());
                }
                Err(err) if err.is_disconnect() && attempt < 8 => {
                    attempt = attempt.saturating_add(1);
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl NodeTransport for StdioTransport {
    async fn send_json(&mut self, value: &Value) -> Result<(), Error> {
        write_json_frame(&mut self.stdin, value, self.max_frame_bytes)
            .await
            .map_err(|err| {
                if err.is_disconnect() {
                    Error::Disconnected
                } else {
                    err
                }
            })
    }

    async fn recv_json(&mut self) -> Result<Option<Value>, Error> {
        match read_json_frame(&mut self.stdout, self.max_frame_bytes).await {
            Ok(None) => Ok(None),
            Ok(Some(value)) => Ok(Some(value)),
            Err(err) if err.is_disconnect() => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn close(&mut self) -> Result<(), Error> {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        Ok(())
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Outbound WebSocket carrier (one WS message = one JSON value).
pub struct WssTransport {
    inner: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

impl WssTransport {
    /// Dial `ws://` or `wss://`.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        Self::connect_with_bearer(url, None).await
    }

    /// Dial Hub `/v1/node` with an optional `Authorization: Bearer` token.
    pub async fn connect_with_bearer(url: &str, token: Option<&str>) -> Result<Self, Error> {
        let mut request = url
            .into_client_request()
            .map_err(|err| Error::WebSocket(err.to_string()))?;
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            let value = format!("Bearer {token}");
            let header = value.parse().map_err(
                |err: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                    Error::WebSocket(format!("authorization header: {err}"))
                },
            )?;
            request.headers_mut().insert(AUTHORIZATION, header);
        }
        let (inner, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|err| Error::WebSocket(err.to_string()))?;
        Ok(Self { inner })
    }
}

impl NodeTransport for WssTransport {
    async fn send_json(&mut self, value: &Value) -> Result<(), Error> {
        let text = serde_json::to_string(value)?;
        self.inner
            .send(Message::text(text))
            .await
            .map_err(|err| Error::WebSocket(err.to_string()))
    }

    async fn recv_json(&mut self) -> Result<Option<Value>, Error> {
        loop {
            match self.inner.next().await {
                None => return Ok(None),
                Some(Err(err)) => return Err(Error::WebSocket(err.to_string())),
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(serde_json::from_str(&text)?));
                }
                Some(Ok(Message::Binary(bytes))) => {
                    return Ok(Some(serde_json::from_slice(&bytes)?));
                }
                Some(Ok(Message::Ping(payload))) => {
                    let _ = self.inner.send(Message::Pong(payload)).await;
                }
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_))) => return Ok(None),
            }
        }
    }

    async fn close(&mut self) -> Result<(), Error> {
        self.inner
            .close(None)
            .await
            .map_err(|err| Error::WebSocket(err.to_string()))
    }
}

/// Default remote argv for the Node protocol carrier.
#[must_use]
pub fn node_stdio_argv(remote_bin: &Path) -> Vec<String> {
    vec![
        remote_bin.to_string_lossy().into_owned(),
        "node".into(),
        "--stdio".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        let backoff = Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(8),
        };
        assert_eq!(backoff.delay(0), Duration::from_secs(1));
        assert_eq!(backoff.delay(1), Duration::from_secs(2));
        assert_eq!(backoff.delay(2), Duration::from_secs(4));
        assert_eq!(backoff.delay(3), Duration::from_secs(8));
        assert_eq!(backoff.delay(4), Duration::from_secs(8));
    }
}
