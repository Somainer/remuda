//! Errors for the Codex app-server stdio client.

use std::path::PathBuf;

use serde_json::Value;

/// Failure from framing, spawn, RPC, or handshake.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// `SpawnSpec.binary` was relative; the path must be pinned absolutely.
    #[error("codex binary path must be absolute: {}", .0.display())]
    RelativeBinary(PathBuf),
    /// `unix://` / `ws://` were requested. Those transports are WebSocket, not JSONL.
    #[error(
        "only --listen stdio:// is supported; unix:// is WebSocket-over-UDS (not JSONL) and ws:// is experimental TCP"
    )]
    UnsupportedListen,
    /// A `-c` override contained a double quote and would break TOML-on-argv quoting.
    #[error("config override must not contain double quotes: {0}")]
    InvalidConfigOverride(String),
    /// `tokio::process::Command` failed before the child started.
    #[error("failed to spawn codex app-server: {0}")]
    Spawn(#[source] std::io::Error),
    /// The child was started without stdin or stdout.
    #[error("codex app-server child is missing a stdio pipe")]
    MissingStdio,
    /// Stdin/stdout I/O failed after spawn.
    #[error("codex app-server I/O error: {0}")]
    Io(#[source] std::io::Error),
    /// A JSON value could not be encoded or decoded.
    #[error("codex app-server JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// One NDJSON line exceeded the configured byte limit.
    #[error("codex app-server NDJSON line exceeds {0} bytes")]
    LineTooLong(usize),
    /// The server returned a JSON-RPC error object for a client request.
    #[error("{method} failed: [{code}] {message}")]
    Rpc {
        /// Client method that was in flight.
        method: String,
        /// Native JSON-RPC error code.
        code: i64,
        /// Native error message. Not a Remuda protocol discriminant.
        message: String,
        /// Optional native `error.data`.
        data: Option<Value>,
    },
    /// The reader task ended while a client request was still pending.
    #[error("codex app-server closed while waiting for {0}")]
    Closed(String),
    /// `initialize` was required and had not completed.
    #[error("codex app-server is not initialized")]
    NotInitialized,
    /// `initialize` was invoked twice on the same connection.
    #[error("codex app-server is already initialized")]
    AlreadyInitialized,
}

impl WireError {
    pub(crate) fn rpc(
        method: impl Into<String>,
        code: i64,
        message: String,
        data: Option<Value>,
    ) -> Self {
        Self::Rpc {
            method: method.into(),
            code,
            message,
            data,
        }
    }
}
