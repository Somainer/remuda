//! Errors from ACP framing, spawn, WebSocket, and JSON-RPC.

use std::io;
use std::path::PathBuf;

use serde_json::Value;

/// Failure talking to an ACP agent over stdio or WebSocket.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying I/O (pipes, sockets, process stdio).
    #[error("acp I/O: {0}")]
    Io(#[from] io::Error),
    /// JSON encode/decode of an NDJSON line or RPC payload.
    #[error("acp JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// One NDJSON line exceeded the codec cap.
    #[error("acp line too long ({len} bytes, max {max})")]
    LineTooLong {
        /// Observed line length.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// `grok` (or configured binary) could not be located.
    #[error("grok binary not found (set GROK_BINARY or put `grok` on PATH): {path}")]
    BinaryNotFound {
        /// Path that was executed.
        path: PathBuf,
    },
    /// Child process failed to start or expose stdio.
    #[error("acp spawn: {0}")]
    Spawn(String),
    /// WebSocket handshake or frame error.
    #[error("acp websocket: {0}")]
    WebSocket(String),
    /// JSON-RPC error object from the agent or the SDK.
    #[error("acp rpc {code}: {message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i32,
        /// Short message.
        message: String,
        /// Optional structured data.
        data: Option<Value>,
    },
    /// Extension method did not start with `_` (after optional `x.ai/` rewrite).
    #[error("acp extension method must start with '_': {0}")]
    ExtMethod(String),
    /// Transport closed before the client finished.
    #[error("acp transport closed")]
    TransportClosed,
    /// Session update channel closed unexpectedly.
    #[error("acp session closed")]
    SessionClosed,
    /// Line was JSON but not ACP JSON-RPC (e.g. grok headless streaming-json).
    #[error("not an ACP JSON-RPC frame")]
    NotAcp,
}

impl Error {
    /// Map an official SDK error into this crate's RPC error.
    #[must_use]
    pub fn from_sdk(err: agent_client_protocol::Error) -> Self {
        Self::Rpc {
            code: i32::from(err.code),
            message: err.message,
            data: err.data,
        }
    }

    /// Convert into an SDK error so `connect_with` callbacks can fail the connection.
    #[must_use]
    pub fn into_sdk(self) -> agent_client_protocol::Error {
        match self {
            Self::Rpc {
                code,
                message,
                data,
            } => agent_client_protocol::Error::new(code, message).data(data),
            other => agent_client_protocol::Error::internal_error().data(other.to_string()),
        }
    }
}

impl From<agent_client_protocol::Error> for Error {
    fn from(err: agent_client_protocol::Error) -> Self {
        Self::from_sdk(err)
    }
}
