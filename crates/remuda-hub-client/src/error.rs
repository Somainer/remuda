//! Failures talking to Hub HTTP or follow WS.

use thiserror::Error;

/// Failures talking to Hub HTTP / WS.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Neither a device token nor a bootstrap token was provided.
    #[error("set a device token or bootstrap token (REMUDA_TOKEN / REMUDA_BOOTSTRAP_TOKEN)")]
    NoCredentials,
    /// Hub returned a non-success status.
    #[error("hub HTTP {status}: {body}")]
    Http {
        /// Status code.
        status: u16,
        /// Response body (no secrets).
        body: String,
    },
    /// `POST /v1/fleet/*` is specified in proposal.md §4.6 but not on this Hub.
    #[error(
        "Hub fleet HTTP is not deployed yet (HTTP {status} on {path}). \
         TODO: POST /v1/fleet/instances, GET /v1/fleet/:id, POST /v1/fleet/:id/commands \
         as specified in docs/design/proposal.md §4.6"
    )]
    FleetUnavailable {
        /// Status code (typically 404).
        status: u16,
        /// Request path.
        path: String,
    },
    /// Client-side placement found no online host.
    #[error("PLACEMENT_UNSATISFIABLE: {0}")]
    Placement(String),
    /// Outbound HTTP.
    #[error("hub request: {0}")]
    Transport(#[from] reqwest::Error),
    /// Response was not JSON.
    #[error("hub JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// WebSocket upgrade or frame.
    #[error("hub websocket: {0}")]
    Websocket(String),
    /// Internal lock / invariant.
    #[error("{0}")]
    Internal(String),
}
