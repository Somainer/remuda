//! Driver and materializer errors mapped onto `protocol.md` §9.1 codes.

use remuda_protocol::{ErrorCode, WireValueError};
use std::io;
use std::path::PathBuf;

/// Failure to pin a binary, materialize a launch, or drive a native session.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// Spec, argv, or overlay could not be turned into a launch recipe.
    #[error("invalid launch spec: {0}")]
    InvalidLaunchSpec(String),
    /// Bypass/yolo is only allowed on human-originated specs. `decisions.md` D-011.
    #[error("bypass permissions (yolo) is not allowed for bot-originated specs")]
    BypassNotAllowedForBot,
    /// Direct provider rotation is v2. `decisions.md` D-012.
    #[error("direct provider delegation is v2")]
    DirectDelegationV2,
    /// A prohibited native mode or env would disable required features.
    #[error("native feature disabled: {0}")]
    NativeFeatureDisabled(String),
    /// Provider ingress does not match the selected driver.
    #[error("provider protocol mismatch: {0}")]
    ProviderProtocolMismatch(String),
    /// Profile is unhealthy, cooling down, or unknown to automatic selection.
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),
    /// Secret broker could not resolve a `secret_ref`.
    #[error("credential unavailable: {0}")]
    CredentialUnavailable(String),
    /// Requested capability is explicitly not provided by this driver.
    #[error("capability unsupported: {0}")]
    CapabilityUnsupported(String),
    /// Requested capability has not been verified for this binary/profile.
    #[error("capability unknown: {0}")]
    CapabilityUnknown(String),
    /// Binary path/version/digest no longer matches the pinned recipe.
    #[error("binary changed: {0}")]
    BinaryChanged(String),
    /// Native session identity is missing or unknown.
    #[error("native session not found")]
    NativeSessionNotFound,
    /// Attach would start or wake a stopped job.
    #[error("attach would wake a stopped job")]
    AttachWouldWake,
    /// No live driver handle is available for control.
    #[error("control unavailable")]
    ControlUnavailable,
    /// Isolated settings could not be written.
    #[error("settings isolation unavailable: {0}")]
    SettingsIsolationUnavailable(String),
    /// Failed to locate an executable.
    #[error("binary not found: {0}")]
    BinaryNotFound(PathBuf),
    /// Filesystem or process IO failed.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// JSON overlay encoding failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Protocol scalar encoding failed.
    #[error("protocol value: {0}")]
    Protocol(#[from] WireValueError),
}

impl DriverError {
    /// Stable protocol error code for Hub/Node mapping.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidLaunchSpec(_) => ErrorCode::InvalidLaunchSpec,
            Self::BypassNotAllowedForBot => ErrorCode::InvalidLaunchSpec,
            Self::DirectDelegationV2 => ErrorCode::CapabilityUnsupported,
            Self::NativeFeatureDisabled(_) => ErrorCode::NativeFeatureDisabled,
            Self::ProviderProtocolMismatch(_) => ErrorCode::ProviderProtocolMismatch,
            Self::ProviderUnavailable(_) => ErrorCode::ProviderUnavailable,
            Self::CredentialUnavailable(_) => ErrorCode::CredentialUnavailable,
            Self::CapabilityUnsupported(_) => ErrorCode::CapabilityUnsupported,
            Self::CapabilityUnknown(_) => ErrorCode::CapabilityUnknown,
            Self::BinaryChanged(_) => ErrorCode::BinaryChanged,
            Self::NativeSessionNotFound => ErrorCode::NativeSessionNotFound,
            Self::AttachWouldWake => ErrorCode::AttachWouldWake,
            Self::ControlUnavailable => ErrorCode::ControlUnavailable,
            Self::SettingsIsolationUnavailable(_) => ErrorCode::SettingsIsolationUnavailable,
            Self::BinaryNotFound(_) => ErrorCode::InvalidLaunchSpec,
            Self::Io(_) | Self::Json(_) | Self::Protocol(_) => ErrorCode::NativeProtocolError,
        }
    }
}

/// Result alias for driver operations.
pub type DriverResult<T> = Result<T, DriverError>;
