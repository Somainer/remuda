//! Spawn `fake-claude` and speak NDJSON on its stdio.

use crate::paths::{FIXED_SESSION_ID, ScriptKind, script_path};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// How to launch [`FakeClaudeProcess`].
#[derive(Clone, Debug)]
pub struct SpawnOptions {
    /// `--session-id` (defaults to [`FIXED_SESSION_ID`]).
    pub session_id: String,
    /// `FAKE_CLAUDE_SCRIPT`: bundled kind or filesystem path.
    pub script: PathBuf,
    /// `FAKE_CLAUDE_TRANSCRIPT_DIR`.
    pub transcript_dir: Option<PathBuf>,
    /// Extra argv after the implicit `-p --output-format stream-json` flags.
    pub extra_args: Vec<String>,
    /// Working directory for the child.
    pub cwd: Option<PathBuf>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            session_id: FIXED_SESSION_ID.to_string(),
            script: script_path(ScriptKind::Ok),
            transcript_dir: None,
            extra_args: Vec::new(),
            cwd: None,
        }
    }
}

impl SpawnOptions {
    /// Playback a bundled script under the fixed session id.
    pub fn bundled(kind: ScriptKind) -> Self {
        Self {
            script: script_path(kind),
            ..Self::default()
        }
    }
}

/// Running `fake-claude` with NDJSON stdin/stdout.
pub struct FakeClaudeProcess {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<std::io::Result<String>>,
    /// `--session-id` value.
    pub session_id: String,
}

impl FakeClaudeProcess {
    /// Write one JSON value as a single NDJSON line.
    pub fn send(&mut self, value: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("fake-claude stdin already closed"))?;
        writeln!(stdin, "{}", serde_json::to_string(value)?)?;
        stdin.flush()?;
        Ok(())
    }

    /// Host `initialize` control request.
    pub fn send_initialize(&mut self, request_id: &str) -> Result<()> {
        self.send(&serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "initialize" }
        }))
    }

    /// User turn with a string prompt.
    pub fn send_user(&mut self, text: &str) -> Result<()> {
        self.send(&serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text }
        }))
    }

    /// Permission / question reply echoing `request_id`.
    pub fn send_control_response(&mut self, request_id: &str, inner: Value) -> Result<()> {
        self.send(&serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": inner
            }
        }))
    }

    /// `interrupt` control request.
    pub fn send_interrupt(&mut self, request_id: &str) -> Result<()> {
        self.send(&serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" }
        }))
    }

    /// Next stdout JSON object, waiting up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Value> {
        match self.lines.recv_timeout(timeout) {
            Ok(Ok(line)) => Ok(serde_json::from_str(&line)?),
            Ok(Err(err)) => Err(err.into()),
            Err(RecvTimeoutError::Timeout) => {
                Err(anyhow!("timed out waiting for fake-claude stdout"))
            }
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("fake-claude stdout closed")),
        }
    }

    /// Collect stdout until `pred` matches, returning the matching frame.
    pub fn recv_until(
        &self,
        timeout: Duration,
        mut pred: impl FnMut(&Value) -> bool,
    ) -> Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("timed out matching fake-claude stdout"));
            }
            let value = self.recv_timeout(remaining)?;
            if pred(&value) {
                return Ok(value);
            }
        }
    }

    /// Close stdin (EOF) and wait for exit.
    pub fn wait(mut self) -> Result<std::process::ExitStatus> {
        self.stdin.take();
        let mut child = self
            .child
            .take()
            .ok_or_else(|| anyhow!("fake-claude child already reaped"))?;
        Ok(child.wait()?)
    }
}

impl Drop for FakeClaudeProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Path to the `fake-claude` binary built for this package.
pub fn fake_claude_bin() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_fake-claude") {
        return PathBuf::from(path);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target");
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    path.push(profile);
    path.push("fake-claude");
    path
}

/// Spawn `fake-claude` with stream-json flags a real host would pass.
pub fn spawn_fake_claude(options: SpawnOptions) -> Result<FakeClaudeProcess> {
    let bin = fake_claude_bin();
    let mut command = Command::new(&bin);
    command
        .arg("-p")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--permission-mode")
        .arg("default")
        .arg("--permission-prompts")
        .arg("host")
        .arg("--permission-prompt-tool")
        .arg("stdio")
        .arg("--session-id")
        .arg(&options.session_id)
        .args(&options.extra_args)
        .env("FAKE_CLAUDE_SCRIPT", &options.script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(dir) = &options.transcript_dir {
        command.env("FAKE_CLAUDE_TRANSCRIPT_DIR", dir);
    }
    if let Some(cwd) = &options.cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    let stdin = child.stdin.take().context("fake-claude stdin")?;
    let stdout = child.stdout.take().context("fake-claude stdout")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    if tx.send(Ok(text)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err));
                    break;
                }
            }
        }
    });
    Ok(FakeClaudeProcess {
        child: Some(child),
        stdin: Some(stdin),
        lines: rx,
        session_id: options.session_id,
    })
}

/// Convenience: frame is a `control_request` with the given subtype.
pub fn is_control_subtype(value: &Value, subtype: &str) -> bool {
    value.get("type").and_then(Value::as_str) == Some("control_request")
        && value.pointer("/request/subtype").and_then(Value::as_str) == Some(subtype)
}

/// Convenience: `type` equals `want`.
pub fn is_type(value: &Value, want: &str) -> bool {
    value.get("type").and_then(Value::as_str) == Some(want)
}

/// Convenience: `type=system` and `subtype` equals `want`.
pub fn is_system_subtype(value: &Value, want: &str) -> bool {
    is_type(value, "system") && value.get("subtype").and_then(Value::as_str) == Some(want)
}

/// Transcript path the fake writes when `FAKE_CLAUDE_TRANSCRIPT_DIR` is set.
pub fn transcript_path(dir: impl AsRef<Path>, session_id: &str) -> PathBuf {
    dir.as_ref().join(format!("{session_id}.jsonl"))
}
