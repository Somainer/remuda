//! Errors for framing, spawn, and the control handshake.

use std::path::PathBuf;

/// Failure from codec, spawn, handshake, or a live Claude process.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying I/O (pipes, spawn, wait).
    #[error("claude-wire I/O: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON object could not be encoded.
    #[error("claude-wire JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// NDJSON line exceeded the configured byte limit.
    #[error("NDJSON line exceeded {limit} bytes")]
    LineTooLong {
        /// Configured maximum line size in bytes.
        limit: usize,
    },
    /// `SpawnSpec` requested a forbidden CLI flag.
    #[error(
        "forbidden Claude flag {flag} (never pass --bare, --no-session-persistence, --cwd, or an argv prompt)"
    )]
    ForbiddenFlag {
        /// The rejected flag token.
        flag: String,
    },
    /// The `claude` binary could not be started.
    #[error("failed to spawn {binary}: {source}")]
    Spawn {
        /// Binary path passed to `Command`.
        binary: PathBuf,
        /// OS error from `Command::spawn`.
        source: std::io::Error,
    },
    /// Child stdin or stdout was not piped.
    #[error("{0} pipe missing after spawn")]
    MissingPipe(&'static str),
    /// Initialize `control_response` did not arrive in time.
    #[error("initialize handshake timed out after {timeout_ms}ms (request_id {request_id})")]
    HandshakeTimeout {
        /// Host-generated initialize `request_id`.
        request_id: String,
        /// Timeout applied to the wait, milliseconds.
        timeout_ms: u64,
    },
    /// Initialize came back as `control_response`/`error`.
    #[error("initialize handshake failed for {request_id}: {message}")]
    HandshakeFailed {
        /// Host-generated initialize `request_id`.
        request_id: String,
        /// CLI error string, or a short local reason.
        message: String,
    },
    /// Stdout closed before initialize completed.
    #[error("claude stdout closed during initialize handshake (request_id {request_id})")]
    HandshakeEof {
        /// Host-generated initialize `request_id`.
        request_id: String,
    },
    /// Writer task is gone (child stdin closed or process exited).
    #[error("claude stdin is closed")]
    StdinClosed,
    /// Outbound channel closed (reader task ended).
    #[error("claude stdout reader ended")]
    StdoutClosed,
    /// Attempted to spawn with an unusable permission mode.
    #[error("permission mode cannot be sent to the CLI")]
    InvalidPermissionMode,
}

impl Error {
    pub(crate) fn forbidden(flag: impl Into<String>) -> Self {
        Self::ForbiddenFlag { flag: flag.into() }
    }
}
