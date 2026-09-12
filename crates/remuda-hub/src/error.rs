//! HTTP and JSON-RPC errors.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use thiserror::Error;

/// Recoverable Hub failures.
#[derive(Debug, Error)]
pub enum HubError {
    /// Missing or invalid device / Node credential.
    #[error("unauthenticated")]
    Unauthenticated,
    /// Origin/Host mismatch or insufficient scope.
    #[error("forbidden")]
    Forbidden,
    /// Target does not exist.
    #[error("not found")]
    NotFound,
    /// Caller sent an unusable body or ID.
    #[error("{0}")]
    BadRequest(String),
    /// Idempotency key reused with a different payload.
    #[error("{0}")]
    Conflict(String),
    /// SQLite or actor mailbox.
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
    /// Internal invariant.
    #[error("{0}")]
    Internal(String),
}

impl HubError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Store(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::Forbidden => "FORBIDDEN",
            Self::NotFound => "NOT_FOUND",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::Conflict(_) => "COMMAND_ID_CONFLICT",
            Self::Store(_) | Self::Internal(_) => "INTERNAL",
        }
    }
}

impl IntoResponse for HubError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = json!({
            "error": self.to_string(),
            "code": self.code(),
        });
        (status, Json(body)).into_response()
    }
}

/// JSON-RPC 2.0 error object for the Node socket.
pub fn rpc_error(id: serde_json::Value, code: i32, message: &str) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// JSON-RPC 2.0 success object.
pub fn rpc_ok(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}
