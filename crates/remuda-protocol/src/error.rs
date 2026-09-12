//! Error wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};

impl ErrorCode {
    /// Stable JSON-RPC server code; protocol §9.1.
    pub const fn rpc_code(self) -> i32 {
        match self {
            Self::Unauthenticated => -32000,
            Self::ScopeDenied => -32001,
            Self::HostOffline => -32002,
            Self::OwnerFenced => -32003,
            Self::ProtocolVersionUnsupported => -32004,
            Self::SchemaVersionUnsupported => -32005,
            Self::CapabilityUnsupported => -32006,
            Self::CapabilityUnknown => -32007,
            Self::NativeFeatureDisabled => -32008,
            Self::BinaryChanged => -32009,
            Self::InvalidLaunchSpec => -32010,
            Self::SettingsIsolationUnavailable => -32011,
            Self::ProviderProtocolMismatch => -32012,
            Self::ProviderUnavailable => -32013,
            Self::CredentialUnavailable => -32014,
            Self::WorkspaceNotFound => -32015,
            Self::WorkspaceBusy => -32016,
            Self::WorktreeBusy => -32017,
            Self::WorktreeDirty => -32018,
            Self::NativeSessionNotFound => -32019,
            Self::NativeSessionOwned => -32020,
            Self::NativeGenerationMismatch => -32021,
            Self::AttachWouldWake => -32022,
            Self::ControlUnavailable => -32023,
            Self::CommandIdConflict => -32024,
            Self::CommandExpired => -32025,
            Self::CommandOutcomeUnknown => -32026,
            Self::RunNotActive => -32027,
            Self::InteractionAlreadyAnswered => -32028,
            Self::InteractionStale => -32029,
            Self::InteractionExpired => -32030,
            Self::InteractionSchemaUnsupported => -32031,
            Self::InteractionNotAnswerable => -32032,
            Self::InvalidAnswer => -32033,
            Self::NativeResponseUnknown => -32034,
            Self::CursorExpired => -32035,
            Self::JournalGap => -32036,
            Self::JournalDiverged => -32037,
            Self::JournalUnavailable => -32038,
            Self::StateUnknown => -32039,
            Self::ResourceLimit => -32040,
            Self::TtyLeaseLost => -32041,
            Self::TtyHistoryGap => -32042,
            Self::ObjectNotFound => -32043,
            Self::ObjectRevisionMismatch => -32044,
            Self::NativeProtocolError => -32045,
            Self::WaitTimeout => -32046,
        }
    }
}

/// ErrorDetails; `protocol.md` §9.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDetails {
    /// `command_id`; protocol §9.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<CommandId>,
    /// `instance_id`; protocol §9.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<InstanceId>,
    /// `interaction_id`; protocol §9.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<InteractionId>,
    /// `expected_generation`; protocol §9.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<U64>,
    /// `actual_generation`; protocol §9.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_generation: Option<U64>,
    /// `evidence_event_ids`; protocol §9.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_event_ids: Option<Vec<EventId>>,
    /// `native_error_ref`; protocol §9.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_error_ref: Option<Id>,
    /// `retry_after_ms`; protocol §9.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u32>,
}

/// RuntimeError; `protocol.md` §9.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeError {
    /// `code`; protocol §9.1.
    pub code: ErrorCode,
    /// `rpc_code`; protocol §9.1.
    pub rpc_code: i32,
    /// `message`; protocol §9.1.
    pub message: String,
    /// `retry`; protocol §9.1.
    pub retry: RetryAction,
    /// `execution`; protocol §9.1.
    pub execution: ExecutionState,
    /// `details`; protocol §9.1.
    pub details: ErrorDetails,
}

/// RpcErrorData; `protocol.md` §9.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RpcErrorData {
    /// `code`; protocol §9.1.
    pub code: ErrorCode,
    /// `retry`; protocol §9.1.
    pub retry: RetryAction,
    /// `execution`; protocol §9.1.
    pub execution: ExecutionState,
    /// `details`; protocol §9.1.
    pub details: ErrorDetails,
}

/// RpcError; `protocol.md` §9.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RpcError {
    /// `code`; protocol §9.1.
    pub code: i32,
    /// `message`; protocol §9.1.
    pub message: String,
    /// `data`; protocol §9.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<RpcErrorData>,
}

impl From<RuntimeError> for RpcError {
    fn from(error: RuntimeError) -> Self {
        Self {
            code: error.code.rpc_code(),
            message: error.message,
            data: Some(RpcErrorData {
                code: error.code,
                retry: error.retry,
                execution: error.execution,
                details: error.details,
            }),
        }
    }
}

/// Mandatory M0 error vocabulary; numeric assignments remain those of `protocol.md` §9.1.
///
/// No member authorizes replay. In particular unknown delivery requires a read-only
/// command query or reconciliation; this list does not implement those operations.
pub const M0_REQUIRED_ERROR_CODES: &[ErrorCode] = &[
    ErrorCode::Unauthenticated,
    ErrorCode::ScopeDenied,
    ErrorCode::HostOffline,
    ErrorCode::OwnerFenced,
    ErrorCode::ProtocolVersionUnsupported,
    ErrorCode::SchemaVersionUnsupported,
    ErrorCode::CapabilityUnsupported,
    ErrorCode::CapabilityUnknown,
    ErrorCode::BinaryChanged,
    ErrorCode::InvalidLaunchSpec,
    ErrorCode::NativeGenerationMismatch,
    ErrorCode::AttachWouldWake,
    ErrorCode::ControlUnavailable,
    ErrorCode::CommandIdConflict,
    ErrorCode::CommandExpired,
    ErrorCode::CommandOutcomeUnknown,
    ErrorCode::NativeResponseUnknown,
    ErrorCode::JournalGap,
    ErrorCode::JournalDiverged,
    ErrorCode::JournalUnavailable,
    ErrorCode::StateUnknown,
    ErrorCode::ResourceLimit,
    ErrorCode::TtyLeaseLost,
    ErrorCode::TtyHistoryGap,
];
