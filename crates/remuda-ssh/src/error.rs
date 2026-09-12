//! SSH, framing, bootstrap, and transport failures.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Failure talking to OpenSSH, framing Node JSON, or pushing a remote binary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying I/O (pipes, files, process stdio).
    #[error("ssh I/O: {0}")]
    Io(#[from] io::Error),
    /// JSON encode/decode of a Node frame.
    #[error("ssh JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// `ssh` / `scp` exited non-zero.
    #[error("ssh failed (status {status:?}): {stderr}")]
    Remote {
        /// Process exit code, if the OS reported one.
        status: Option<i32>,
        /// Captured stderr (no private key material).
        stderr: String,
    },
    /// `ssh -G` output was missing a required keyword.
    #[error("ssh -G parse: {0}")]
    Parse(String),
    /// `~/.ssh/config` (or a test fixture) could not be read as Host entries.
    #[error("ssh config {path}: {detail}")]
    Config {
        /// Config path that failed.
        path: PathBuf,
        /// Short reason.
        detail: String,
    },
    /// JSON payload exceeded the length-prefix cap.
    #[error("JSON frame {len} bytes exceeds max {max}")]
    FrameTooLarge {
        /// Encoded JSON size.
        len: u32,
        /// Configured maximum.
        max: u32,
    },
    /// Stream ended in the middle of a length prefix or payload.
    #[error("truncated length-prefixed JSON frame")]
    TruncatedFrame,
    /// Length prefix was zero.
    #[error("empty length-prefixed JSON frame")]
    EmptyFrame,
    /// Local musl (or configured) remuda binary is missing.
    #[error("local remuda binary not found: {0}")]
    BinaryNotFound(PathBuf),
    /// Remote sha256 did not match the local digest after upload.
    #[error("remote digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        /// Local `sha256:` digest.
        expected: String,
        /// Remote digest (or `missing`).
        actual: String,
    },
    /// A subprocess exceeded its timeout.
    #[error("ssh timed out after {0:?}")]
    Timeout(Duration),
    /// WebSocket handshake or frame error.
    #[error("ssh websocket: {0}")]
    WebSocket(String),
    /// SSH child or WebSocket closed while a session was expected to stay up.
    #[error("node transport disconnected")]
    Disconnected,
    /// Remote path contained a NUL, newline, or other rejected byte.
    #[error("unsafe remote path: {0}")]
    InvalidPath(String),
    /// `ssh` binary could not be located.
    #[error("ssh binary not found: {0}")]
    SshNotFound(PathBuf),
    /// `node.hello` could not be enrolled with Hub.
    #[error("hub enroll: {0}")]
    Enroll(String),
}

impl Error {
    /// True when the peer (ssh child or socket) went away.
    #[must_use]
    pub fn is_disconnect(&self) -> bool {
        matches!(
            self,
            Self::Disconnected | Self::Io(_) | Self::Remote { .. } | Self::TruncatedFrame
        )
    }

    pub(crate) fn parse(msg: impl Into<String>) -> Self {
        Self::Parse(msg.into())
    }

    pub(crate) fn remote(status: Option<i32>, stderr: impl Into<String>) -> Self {
        Self::Remote {
            status,
            stderr: trim_stderr(stderr.into()),
        }
    }
}

pub(crate) fn trim_stderr(stderr: String) -> String {
    const MAX: usize = 8 * 1024;
    let mut text = stderr.replace('\0', "");
    if text.len() > MAX {
        text.truncate(MAX);
        text.push('…');
    }
    text
}
