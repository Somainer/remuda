//! Errors at the local Node boundary.

use remuda_protocol::WireValueError;

/// Failure returned by the local Node composition and API.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The requested entity does not exist.
    #[error("{entity} not found: {id}")]
    NotFound {
        /// Entity class.
        entity: &'static str,
        /// Wire identity.
        id: String,
    },
    /// An identity or idempotency key conflicts with existing state.
    #[error("state conflict: {0}")]
    Conflict(String),
    /// Request input is malformed or unsupported.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Server configuration is unsafe or incomplete.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// An instance-local bounded queue cannot accept more work.
    #[error("instance command queue is full")]
    QueueFull,
    /// The instance worker is no longer reachable.
    #[error("instance driver task is unavailable")]
    DriverUnavailable,
    /// A driver rejected or failed an operation.
    #[error("driver error: {0}")]
    Driver(String),
    /// Interaction deadline already passed.
    #[error("interaction expired")]
    InteractionExpired,
    /// A different commandId already committed the unique answer.
    #[error("interaction already answered by {winner}")]
    InteractionSuperseded {
        /// Winning command identity.
        winner: String,
    },
    /// Shared in-memory state was poisoned by a panic.
    #[error("local store lock poisoned")]
    StorePoisoned,
    /// A wire value could not be constructed.
    #[error("wire value error: {0}")]
    Wire(#[from] WireValueError),
    /// JSON encoding or decoding failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Socket, file, or listener I/O failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Outbound Hub WebSocket failed.
    #[error("hub transport: {0}")]
    Transport(String),
    /// Hub JSON-RPC error object.
    #[error("hub rpc [{code}]: {message}")]
    HubRpc {
        /// Native JSON-RPC code.
        code: i64,
        /// Native message; not a Remuda discriminant.
        message: String,
    },
    /// Hub control plane closed.
    #[error("hub connection closed")]
    Disconnected,
    /// Journal append waiters were dropped because the session ended.
    #[error("journal append queue closed")]
    JournalQueueClosed,
}
