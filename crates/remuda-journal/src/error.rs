//! Journal errors.

use remuda_protocol::WireValueError;
use std::path::PathBuf;

/// Failure to persist, tail, or fold observations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite index failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Filesystem failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encode/decode failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Protocol scalar or identity encoding failure.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Writer thread is gone.
    #[error("journal writer closed")]
    Closed,
    /// Envelope `instanceId` did not match the append target.
    #[error("envelope instance {envelope} does not match append target {target}")]
    InstanceMismatch {
        /// Target passed to `append`.
        target: String,
        /// Envelope field.
        envelope: String,
    },
    /// Stored journal identity does not match the envelope.
    #[error("journal id diverged for instance {instance}")]
    JournalMismatch {
        /// Instance whose journal identity changed.
        instance: String,
    },
    /// JSONL and SQLite seq no longer describe the same log.
    #[error("journal diverged for {instance} at seq {seq}")]
    Diverged {
        /// Instance identity.
        instance: String,
        /// Conflicting sequence number.
        seq: u64,
    },
    /// `follow`/`read_range` started past the durable watermark plus one.
    #[error("seq gap for {instance}: from {from_seq} durable {durable_seq}")]
    Gap {
        /// Instance identity.
        instance: String,
        /// Requested start.
        from_seq: u64,
        /// Durable watermark.
        durable_seq: u64,
    },
    /// Follow subscriber lagged past the in-memory buffer.
    #[error("follow buffer overflow for {0}")]
    FollowOverflow(String),
    /// A required path was missing or not a file.
    #[error("path not found: {0}")]
    Path(PathBuf),
}

impl From<WireValueError> for Error {
    fn from(value: WireValueError) -> Self {
        Self::Protocol(value.to_string())
    }
}

impl Error {
    pub(crate) fn closed_send<T>(_: tokio::sync::mpsc::error::SendError<T>) -> Self {
        Self::Closed
    }
}
