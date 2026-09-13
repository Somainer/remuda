//! Errors from the Herdr socket API, process spawn, and terminal bridge.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Failure talking to a Herdr server or its terminal-session child.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying I/O (connect, read, write, process pipes).
    #[error("herdr I/O: {0}")]
    Io(#[from] io::Error),
    /// JSON encode/decode of an NDJSON line.
    #[error("herdr JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Herdr returned `{"error":{code,message}}`.
    #[error("herdr {method}: {code}: {message}")]
    Api {
        /// RPC method that failed.
        method: String,
        /// Herdr error code (`invalid_request`, `agent_not_ready`, …).
        code: String,
        /// Human-readable message from Herdr.
        message: String,
    },
    /// The Unix socket closed before a matching response arrived.
    ///
    /// The request was already written, so its outcome is **unknown** and it
    /// must not be retried blindly.
    #[error("herdr disconnected ({socket})")]
    Disconnected {
        /// Socket path that closed.
        socket: PathBuf,
    },
    /// The socket could not be dialed at all (missing, refused, or unlinked
    /// by a server that just exited).
    ///
    /// Nothing was written, so the request is safe to retry.
    #[error("herdr unreachable ({socket})")]
    Unreachable {
        /// Socket path that could not be dialed.
        socket: PathBuf,
    },
    /// Request exceeded the client timeout.
    #[error("herdr request `{method}` timed out after {timeout:?}")]
    Timeout {
        /// RPC method.
        method: String,
        /// Timeout applied.
        timeout: Duration,
    },
    /// Success payload `type` did not match the typed helper.
    #[error("herdr unexpected result type `{found}` (wanted `{wanted}`)")]
    UnexpectedResult {
        /// Expected `result.type`.
        wanted: &'static str,
        /// Actual `result.type` or `<missing>`.
        found: String,
    },
    /// Headless `herdr server` did not become reachable.
    #[error("herdr server `{session}` failed to start: {detail}")]
    ServerStart {
        /// Isolated session name.
        session: String,
        /// Combined stderr / wait detail.
        detail: String,
    },
    /// Caller asked for the user default session without saying so.
    #[error(
        "refusing the default herdr socket {socket}; pass session_name \"default\" to use it explicitly"
    )]
    DefaultSessionGuard {
        /// Path that would have been used.
        socket: PathBuf,
    },
    /// `herdr` binary could not be located.
    #[error("herdr binary not found (set HERDR_BINARY or put `herdr` on PATH)")]
    BinaryNotFound,
    /// Terminal-session child printed a `terminal.closed` envelope.
    #[error("herdr terminal closed: {reason}")]
    TerminalClosed {
        /// Reason string from Herdr.
        reason: String,
    },
    /// Terminal-session stdout ended without a close envelope.
    #[error("herdr terminal session ended unexpectedly")]
    TerminalEof,
    /// `terminal.frame.bytes` was not valid standard base64.
    #[error("herdr terminal frame base64: {0}")]
    Base64(String),
    /// Control command could not be written because the observer is read-only.
    #[error("herdr terminal observer is read-only; open control mode to write or resize")]
    ReadOnlyTerminal,
}

/// Herdr's error code for a server that is winding down and refuses work.
///
/// Observed on herdr 0.9.0 between `server.stop` (or `SIGTERM`) and process
/// exit: `ping` still answers, every other method returns this.
pub const SERVER_UNAVAILABLE: &str = "server_unavailable";

impl Error {
    /// True when the socket went away or the peer closed the stream.
    #[must_use]
    pub fn is_disconnect(&self) -> bool {
        matches!(
            self,
            Self::Disconnected { .. } | Self::Unreachable { .. } | Self::Io(_)
        )
    }

    /// True when the server accepted the connection but refused the request
    /// because it is shutting down.
    ///
    /// Herdr answers `ping` throughout this window, so this is the only
    /// reliable way to tell a live server from a dying one.
    #[must_use]
    pub fn is_server_unavailable(&self) -> bool {
        match self {
            Self::Api { code, message, .. } => {
                code == SERVER_UNAVAILABLE || message.contains("server is shutting down")
            }
            _ => false,
        }
    }

    /// True when a server restart can still resolve this and the request is
    /// safe to send again: the server refused it because it is shutting down,
    /// or the socket could not be dialed at all.
    ///
    /// Both cases prove the request was **not** dispatched. [`Self::Disconnected`]
    /// and [`Self::Timeout`] are deliberately excluded — they leave the
    /// outcome unknown, and replaying a prompt is worse than reporting it.
    #[must_use]
    pub fn is_transient_carrier(&self) -> bool {
        match self {
            Self::Api { .. } => self.is_server_unavailable(),
            Self::Unreachable { .. } => true,
            Self::Io(error) => matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ),
            _ => false,
        }
    }
}
