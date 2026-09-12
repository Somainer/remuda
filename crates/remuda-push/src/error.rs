//! Push errors.

use std::path::PathBuf;

/// Failure to persist keys, store a subscription, encrypt, or deliver.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON encode/decode failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Invalid VAPID key file or subscription material.
    #[error("{0}")]
    Invalid(String),
    /// Browser subscription rejected before persistence.
    #[error("invalid push subscription")]
    InvalidSubscription,
    /// web-push encryption or VAPID signing failed.
    #[error("web-push: {0}")]
    WebPush(String),
    /// Outbound HTTP to the push service failed.
    #[error("push delivery failed")]
    Delivery,
    /// Push service returned a non-success status that is not gone.
    #[error("push endpoint returned status {0}")]
    Status(u16),
    /// Endpoint is gone (404/410) and should be pruned.
    #[error("push endpoint gone")]
    Gone,
    /// A required path was missing.
    #[error("path not found: {0}")]
    Path(PathBuf),
}

impl From<web_push::WebPushError> for Error {
    fn from(value: web_push::WebPushError) -> Self {
        match value {
            web_push::WebPushError::EndpointNotValid(_)
            | web_push::WebPushError::EndpointNotFound(_) => Self::Gone,
            other => Self::WebPush(other.to_string()),
        }
    }
}
