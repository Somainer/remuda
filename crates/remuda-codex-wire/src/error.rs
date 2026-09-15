//! Errors for the Codex app-server stdio client.

use std::path::PathBuf;

use serde_json::Value;

/// JSON-RPC error payload boxed inside [`WireError::Rpc`] so `Result<_, WireError>`
/// stays under clippy's `result_large_err` limit.
#[derive(Debug, thiserror::Error)]
#[error("{method} failed: [{code}] {message}")]
pub struct RpcError {
    /// Client method that was in flight.
    pub method: String,
    /// Native JSON-RPC error code.
    pub code: i64,
    /// Native error message. Not a Remuda protocol discriminant.
    pub message: String,
    /// Optional native `error.data`.
    pub data: Option<Value>,
}

/// Failure from framing, spawn, RPC, or handshake.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// `SpawnSpec.binary` was relative; the path must be pinned absolutely.
    #[error("codex binary path must be absolute: {}", .0.display())]
    RelativeBinary(PathBuf),
    /// A `-c` override contained a double quote and would break TOML-on-argv quoting.
    #[error("config override must not contain double quotes: {0}")]
    InvalidConfigOverride(String),
    /// A `model_reasoning_effort` value was not in codex's verified vocabulary.
    #[error("unknown codex model_reasoning_effort {0:?} (one of: minimal/low/medium/high/xhigh)")]
    InvalidReasoningEffort(String),
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
    #[error(transparent)]
    Rpc(Box<RpcError>),
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
        Self::Rpc(Box::new(RpcError {
            method: method.into(),
            code,
            message,
            data,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_error_fits_result_large_err() {
        assert!(
            std::mem::size_of::<WireError>() < 128,
            "WireError is {} bytes",
            std::mem::size_of::<WireError>()
        );
    }

    #[test]
    fn rpc_display_keeps_method_code_message() {
        let error = WireError::rpc("turn/start", -32603, "boom".into(), None);
        assert_eq!(error.to_string(), "turn/start failed: [-32603] boom");
    }
}
