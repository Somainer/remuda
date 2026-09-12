//! `herdr terminal session observe|control` subprocess bridge.
//!
//! Stdout is NDJSON `terminal.frame` with base64 ANSI; this type yields decoded
//! [`Bytes`]. Control mode accepts stdin JSON `terminal.input` / `terminal.resize`.

use std::process::Stdio;

use base64::Engine;
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tracing::debug;

use crate::client::Client;
use crate::error::Error;

/// Observe (read-only, server crops) vs control (may resize the PTY).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalMode {
    /// `herdr terminal session observe`.
    Observe,
    /// `herdr terminal session control`.
    Control,
}

/// Options for [`TerminalObserver::open`].
#[derive(Debug, Clone)]
pub struct TerminalOpen {
    /// Pane id (`w1:p2`).
    pub pane_id: String,
    /// Requested columns.
    pub cols: u16,
    /// Requested rows.
    pub rows: u16,
    /// Observe vs control.
    pub mode: TerminalMode,
    /// `--takeover` (control only). Default false — do not steal another viewer.
    pub takeover: bool,
}

impl TerminalOpen {
    /// Read-only observer.
    #[must_use]
    pub fn observe(pane_id: impl Into<String>, cols: u16, rows: u16) -> Self {
        Self {
            pane_id: pane_id.into(),
            cols,
            rows,
            mode: TerminalMode::Observe,
            takeover: false,
        }
    }

    /// Writable controller (PTY resize). `takeover` stays false.
    #[must_use]
    pub fn control(pane_id: impl Into<String>, cols: u16, rows: u16) -> Self {
        Self {
            pane_id: pane_id.into(),
            cols,
            rows,
            mode: TerminalMode::Control,
            takeover: false,
        }
    }
}

/// One decoded ANSI frame from the observe/control child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFrame {
    /// Monotonic seq from Herdr.
    pub seq: u64,
    /// Cell width.
    pub width: u16,
    /// Cell height.
    pub height: u16,
    /// Full-frame reset (vs damage).
    pub full: bool,
    /// Raw ANSI bytes (base64 already decoded).
    pub bytes: Bytes,
}

/// Wire envelope from `herdr terminal session observe|control` stdout.
#[derive(Debug, Clone, Deserialize)]
pub struct TerminalEnvelope {
    /// `terminal.frame` or `terminal.closed`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Frame seq.
    #[serde(default)]
    pub seq: u64,
    /// Encoding (`ansi`).
    #[serde(default)]
    pub encoding: Option<String>,
    /// Width.
    #[serde(default)]
    pub width: u16,
    /// Height.
    #[serde(default)]
    pub height: u16,
    /// Full frame.
    #[serde(default)]
    pub full: bool,
    /// Base64 ANSI.
    #[serde(default)]
    pub bytes: Option<String>,
    /// Close reason.
    #[serde(default)]
    pub reason: Option<String>,
}

impl TerminalEnvelope {
    /// Decode a `terminal.frame` line into ANSI bytes.
    pub fn into_frame(self) -> Result<TerminalFrame, Error> {
        if self.kind == "terminal.closed" {
            return Err(Error::TerminalClosed {
                reason: self.reason.unwrap_or_else(|| "closed".into()),
            });
        }
        if self.kind != "terminal.frame" {
            return Err(Error::UnexpectedResult {
                wanted: "terminal.frame",
                found: self.kind,
            });
        }
        let encoded = self.bytes.unwrap_or_default();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(|err| Error::Base64(err.to_string()))?;
        Ok(TerminalFrame {
            seq: self.seq,
            width: self.width,
            height: self.height,
            full: self.full,
            bytes: Bytes::from(raw),
        })
    }
}

/// Control-mode stdin command.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum TerminalCommand {
    /// Inject UTF-8 text.
    #[serde(rename = "terminal.input")]
    InputText {
        /// Text.
        text: String,
    },
    /// Inject raw bytes (base64).
    #[serde(rename = "terminal.input")]
    InputBytes {
        /// Standard-base64 payload.
        bytes: String,
    },
    /// Resize the PTY (control mode only).
    #[serde(rename = "terminal.resize")]
    Resize {
        /// Columns.
        cols: u16,
        /// Rows.
        rows: u16,
    },
    /// Release the controller.
    #[serde(rename = "terminal.release")]
    Release,
}

/// Subprocess wrapping `herdr terminal session observe|control`.
pub struct TerminalObserver {
    child: Child,
    stdin: Option<ChildStdin>,
    mode: TerminalMode,
    frames: mpsc::Receiver<Result<TerminalFrame, Error>>,
}

impl TerminalObserver {
    /// Spawn observe (read-only) for `pane_id`.
    pub async fn open(client: &Client, pane_id: &str, cols: u16, rows: u16) -> Result<Self, Error> {
        Self::spawn(client, TerminalOpen::observe(pane_id, cols, rows)).await
    }

    /// Spawn control (writable, resize) for `pane_id`.
    pub async fn open_control(
        client: &Client,
        pane_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Self, Error> {
        Self::spawn(client, TerminalOpen::control(pane_id, cols, rows)).await
    }

    /// Spawn from explicit options.
    pub async fn spawn(client: &Client, open: TerminalOpen) -> Result<Self, Error> {
        let mut command = Command::new(client.binary());
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_PANE_ID");
        if let Some(name) = client.session_name() {
            command.arg("--session").arg(name);
        } else {
            command.env("HERDR_SOCKET_PATH", client.socket_path());
        }
        let mode = match open.mode {
            TerminalMode::Observe => "observe",
            TerminalMode::Control => "control",
        };
        command
            .arg("terminal")
            .arg("session")
            .arg(mode)
            .arg(&open.pane_id);
        if open.takeover && matches!(open.mode, TerminalMode::Control) {
            command.arg("--takeover");
        }
        command
            .arg("--cols")
            .arg(open.cols.to_string())
            .arg("--rows")
            .arg(open.rows.to_string());

        let mut child = command.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::BinaryNotFound
            } else {
                Error::Io(err)
            }
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Error::Io(std::io::Error::other(
                "herdr terminal session has no stdout",
            ))
        })?;
        let stdin = child.stdin.take();
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(read_frames(stdout, tx));
        Ok(Self {
            child,
            stdin,
            mode: open.mode,
            frames: rx,
        })
    }

    /// Next decoded ANSI frame.
    pub async fn next_frame(&mut self) -> Option<Result<TerminalFrame, Error>> {
        self.frames.recv().await
    }

    /// Stream of decoded ANSI [`Bytes`] (drops metadata).
    pub fn bytes_stream(&mut self) -> impl Stream<Item = Result<Bytes, Error>> + '_ {
        futures::stream::unfold(self, |observer| async move {
            match observer.next_frame().await? {
                Ok(frame) => Some((Ok(frame.bytes), observer)),
                Err(err) => Some((Err(err), observer)),
            }
        })
    }

    /// Resize the PTY. No-op error on observe mode.
    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), Error> {
        self.send(&TerminalCommand::Resize { cols, rows }).await
    }

    /// Inject UTF-8 into the PTY (control mode).
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<(), Error> {
        self.send(&TerminalCommand::InputText { text: text.into() })
            .await
    }

    /// Inject raw bytes (control mode).
    pub async fn send_bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.send(&TerminalCommand::InputBytes { bytes: encoded })
            .await
    }

    async fn send(&mut self, command: &TerminalCommand) -> Result<(), Error> {
        if matches!(self.mode, TerminalMode::Observe)
            && !matches!(command, TerminalCommand::Release)
        {
            return Err(Error::ReadOnlyTerminal);
        }
        let stdin = self.stdin.as_mut().ok_or(Error::ReadOnlyTerminal)?;
        let mut line = serde_json::to_vec(command)?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }
}

impl Drop for TerminalObserver {
    fn drop(&mut self) {
        // This child is `herdr terminal session observe/control`, never the
        // Herdr daemon or the pane PTY. Killing it does not stop the agent.
        if let Some(pid) = self.child.id() {
            debug!(pid, "dropping herdr terminal session child");
        }
        let _ = self.child.start_kill();
    }
}

async fn read_frames(
    stdout: tokio::process::ChildStdout,
    tx: mpsc::Sender<Result<TerminalFrame, Error>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match serde_json::from_str::<TerminalEnvelope>(&line) {
                Ok(envelope) => match envelope.into_frame() {
                    Ok(frame) => {
                        if tx.send(Ok(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err)).await;
                        break;
                    }
                },
                Err(err) => {
                    let _ = tx.send(Err(Error::Json(err))).await;
                    break;
                }
            },
            Ok(None) => {
                let _ = tx.send(Err(Error::TerminalEof)).await;
                break;
            }
            Err(err) => {
                let _ = tx.send(Err(Error::Io(err))).await;
                break;
            }
        }
    }
}

/// Parse a fixture / captured JSONL line into a frame. Used by unit tests.
pub fn parse_terminal_line(line: &str) -> Result<TerminalFrame, Error> {
    let value: Value = serde_json::from_str(line)?;
    let envelope: TerminalEnvelope = serde_json::from_value(value)?;
    envelope.into_frame()
}
