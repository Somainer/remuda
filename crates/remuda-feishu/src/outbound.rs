//! `lark-cli im +messages-send` / `+messages-reply` wrapper. DryRun records argv only.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::debug;

use crate::error::Error;

/// Whether the wrapper executes `lark-cli` or only records the planned argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Record argv; never spawn. Default for tests and Hub dry paths.
    DryRun,
    /// Spawn `lark-cli`. Callers must pass `--confirm` equivalent at a higher layer.
    Live,
}

/// One planned `lark-cli` invocation (no env secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCommand {
    /// Binary plus flags, including `--idempotency-key`.
    pub argv: Vec<String>,
}

/// Where the message should land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundTarget {
    /// `+messages-send --chat-id`.
    Send {
        /// Chat id (`oc_…`).
        chat_id: String,
    },
    /// `+messages-reply --message-id`, optionally `--reply-in-thread`.
    Reply {
        /// Parent message id (`om_…`).
        message_id: String,
        /// Required for topic groups / thread replies so we do not open a new topic.
        in_thread: bool,
    },
}

/// Body of an outbound IM call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundBody {
    /// `--text` (preferred for logs/code; `--markdown` rewrites headings).
    Text(String),
    /// `--markdown` (becomes `post`).
    Markdown(String),
    /// `--msg-type interactive --content <card JSON 2.0>`.
    Interactive(Value),
    /// `--file` cwd-relative path.
    File {
        /// Path as passed to lark-cli (must be cwd-relative).
        path: PathBuf,
    },
}

/// Result of send/reply. DryRun uses a placeholder message id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundReceipt {
    /// Native message id when the CLI returned one.
    pub message_id: Option<String>,
    /// The argv that was recorded or executed.
    pub planned: PlannedCommand,
    /// True when the process actually ran.
    pub executed: bool,
}

/// Subprocess facade around `lark-cli im`.
#[derive(Debug, Clone)]
pub struct LarkCli {
    binary: PathBuf,
    profile: Option<String>,
    as_identity: String,
    mode: ExecutionMode,
    timeout: Duration,
    recorded: Vec<PlannedCommand>,
}

impl Default for LarkCli {
    fn default() -> Self {
        Self::dry_run()
    }
}

impl LarkCli {
    /// Record commands only.
    #[must_use]
    pub fn dry_run() -> Self {
        Self {
            binary: PathBuf::from("lark-cli"),
            profile: None,
            as_identity: "bot".into(),
            mode: ExecutionMode::DryRun,
            timeout: Duration::from_secs(30),
            recorded: Vec::new(),
        }
    }

    /// Execute `binary` (`lark-cli` on PATH, or a test double).
    #[must_use]
    pub fn live(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            profile: None,
            as_identity: "bot".into(),
            mode: ExecutionMode::Live,
            timeout: Duration::from_secs(30),
            recorded: Vec::new(),
        }
    }

    /// `lark-cli --profile`.
    #[must_use]
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Identity flag; dispatcher always uses `bot`.
    #[must_use]
    pub fn with_as(mut self, identity: impl Into<String>) -> Self {
        self.as_identity = identity.into();
        self
    }

    /// Live-mode process timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Recorded argv, oldest first.
    #[must_use]
    pub fn recorded(&self) -> &[PlannedCommand] {
        &self.recorded
    }

    /// Execution mode.
    #[must_use]
    pub fn mode(&self) -> ExecutionMode {
        self.mode
    }

    /// Send a text message to a chat.
    pub async fn send_text(
        &mut self,
        chat_id: &str,
        text: &str,
        seed: &str,
    ) -> Result<OutboundReceipt, Error> {
        self.dispatch(
            OutboundTarget::Send {
                chat_id: chat_id.to_string(),
            },
            OutboundBody::Text(text.to_string()),
            seed,
        )
        .await
    }

    /// Send a Card JSON 2.0 object as `interactive` content (no `msg_type` wrapper).
    pub async fn send_card(
        &mut self,
        chat_id: &str,
        card: &Value,
        seed: &str,
    ) -> Result<OutboundReceipt, Error> {
        crate::validate_card(card)?;
        self.dispatch(
            OutboundTarget::Send {
                chat_id: chat_id.to_string(),
            },
            OutboundBody::Interactive(card.clone()),
            seed,
        )
        .await
    }

    /// Send a cwd-relative file (long reports; cards are capped at 30 KB).
    pub async fn send_file(
        &mut self,
        chat_id: &str,
        path: &Path,
        seed: &str,
    ) -> Result<OutboundReceipt, Error> {
        self.dispatch(
            OutboundTarget::Send {
                chat_id: chat_id.to_string(),
            },
            OutboundBody::File {
                path: path.to_path_buf(),
            },
            seed,
        )
        .await
    }

    /// Reply, optionally in-thread.
    pub async fn reply(
        &mut self,
        message_id: &str,
        body: OutboundBody,
        in_thread: bool,
        seed: &str,
    ) -> Result<OutboundReceipt, Error> {
        if let OutboundBody::Interactive(card) = &body {
            crate::validate_card(card)?;
        }
        self.dispatch(
            OutboundTarget::Reply {
                message_id: message_id.to_string(),
                in_thread,
            },
            body,
            seed,
        )
        .await
    }

    /// Reply in the current topic (`--reply-in-thread`).
    pub async fn reply_in_thread(
        &mut self,
        message_id: &str,
        body: OutboundBody,
        seed: &str,
    ) -> Result<OutboundReceipt, Error> {
        self.reply(message_id, body, true, seed).await
    }

    async fn dispatch(
        &mut self,
        target: OutboundTarget,
        body: OutboundBody,
        seed: &str,
    ) -> Result<OutboundReceipt, Error> {
        if let OutboundBody::File { path } = &body {
            check_relative_file(path)?;
        }
        let argv = self.build_argv(&target, &body, seed)?;
        let planned = PlannedCommand { argv: argv.clone() };
        self.recorded.push(planned.clone());
        match self.mode {
            ExecutionMode::DryRun => {
                debug!(argc = argv.len(), "feishu outbound dry-run");
                Ok(OutboundReceipt {
                    message_id: Some("om_dry_run".into()),
                    planned,
                    executed: false,
                })
            }
            ExecutionMode::Live => {
                let receipt = self.exec(&argv).await?;
                Ok(OutboundReceipt {
                    message_id: receipt,
                    planned,
                    executed: true,
                })
            }
        }
    }

    fn build_argv(
        &self,
        target: &OutboundTarget,
        body: &OutboundBody,
        seed: &str,
    ) -> Result<Vec<String>, Error> {
        let mut argv = vec![self.binary.display().to_string()];
        if let Some(profile) = &self.profile {
            argv.push("--profile".into());
            argv.push(profile.clone());
        }
        argv.push("im".into());
        match target {
            OutboundTarget::Send { chat_id } => {
                argv.push("+messages-send".into());
                argv.push("--chat-id".into());
                argv.push(chat_id.clone());
            }
            OutboundTarget::Reply {
                message_id,
                in_thread,
            } => {
                argv.push("+messages-reply".into());
                argv.push("--message-id".into());
                argv.push(message_id.clone());
                if *in_thread {
                    argv.push("--reply-in-thread".into());
                }
            }
        }
        push_body(&mut argv, body);
        argv.push("--as".into());
        argv.push(self.as_identity.clone());
        argv.push("--idempotency-key".into());
        argv.push(idempotency_key(seed));
        Ok(argv)
    }

    async fn exec(&self, argv: &[String]) -> Result<Option<String>, Error> {
        if argv.is_empty() {
            return Err(Error::BinaryNotFound(self.binary.clone()));
        }
        if !self.binary.as_os_str().is_empty() && self.binary.is_absolute() && !self.binary.exists()
        {
            return Err(Error::BinaryNotFound(self.binary.clone()));
        }
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn()?;
        let output = timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| Error::CliTimeout)??;
        if !output.status.success() {
            return Err(Error::Cli {
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(extract_message_id(&stdout))
    }
}

fn push_body(argv: &mut Vec<String>, body: &OutboundBody) {
    match body {
        OutboundBody::Text(text) => {
            argv.push("--text".into());
            argv.push(text.clone());
        }
        OutboundBody::Markdown(md) => {
            argv.push("--markdown".into());
            argv.push(md.clone());
        }
        OutboundBody::Interactive(card) => {
            argv.push("--msg-type".into());
            argv.push("interactive".into());
            argv.push("--content".into());
            argv.push(card.to_string());
        }
        OutboundBody::File { path } => {
            argv.push("--file".into());
            argv.push(path.display().to_string());
        }
    }
}

/// lark-cli `--idempotency-key` is at most 50 characters; 1 hour window.
#[must_use]
pub fn idempotency_key(seed: &str) -> String {
    let mut out = String::from("r");
    for ch in seed.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        }
        if out.len() >= 50 {
            break;
        }
    }
    if out.len() == 1 {
        out.push('0');
    }
    out
}

fn check_relative_file(path: &Path) -> Result<(), Error> {
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(Error::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn extract_message_id(stdout: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    value
        .pointer("/data/message_id")
        .or_else(|| value.get("message_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_fits_lark_cli_limit() {
        let key = idempotency_key(&"om_".repeat(40));
        assert!(key.len() <= 50);
        assert!(key.starts_with('r'));
    }

    #[test]
    fn rejects_parent_dir_file() {
        assert!(check_relative_file(Path::new("../secret")).is_err());
        assert!(check_relative_file(Path::new("/tmp/x")).is_err());
        assert!(check_relative_file(Path::new("reports/out.md")).is_ok());
    }
}
