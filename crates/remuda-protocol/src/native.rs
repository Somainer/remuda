//! Native wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};

/// TranscriptRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptRef {
    /// `object_id`; protocol §1.3.
    pub object_id: Id,
    /// `source_path`; protocol §1.3.
    pub source_path: String,
}

/// CodexRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRef {
    /// `thread_id`; protocol §1.3.
    pub thread_id: String,
}

/// AcpRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpRef {
    /// `session_id`; protocol §1.3.
    pub session_id: String,
    /// `protocol_version`; protocol §1.3.
    pub protocol_version: u32,
}

/// ClaudeRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeRef {
    /// `session_id`; protocol §1.3.
    pub session_id: String,
    /// `background_job_id`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_job_id: Option<String>,
}

/// AgyRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgyRef {
    /// `conversation_id`; protocol §1.3.
    pub conversation_id: String,
}

/// HerdrRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HerdrRef {
    /// `server_identity`; protocol §1.3.
    pub server_identity: Id,
    /// `server_epoch`; protocol §1.3.
    pub server_epoch: Id,
    /// `pane_id`; protocol §1.3.
    pub pane_id: String,
}

/// NativeRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeRef {
    /// `host_id`; protocol §1.3.
    pub host_id: HostId,
    /// `native_store_id`; protocol §1.3.
    pub native_store_id: Id,
    /// `kind`; protocol §1.3.
    pub kind: AgentKind,
    /// `session_id`; protocol §1.3.
    pub session_id: Knowledge<String>,
    /// `transcript`; protocol §1.3.
    pub transcript: Knowledge<TranscriptRef>,
    /// `codex`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexRef>,
    /// `acp`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpRef>,
    /// `claude`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeRef>,
    /// `agy`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agy: Option<AgyRef>,
    /// `herdr`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr: Option<HerdrRef>,
}

/// ProcessIdentity; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    /// `pid`; protocol §1.3.
    pub pid: u32,
    /// `birth_id`; protocol §1.3.
    pub birth_id: String,
    /// `supervisor_id`; protocol §1.3.
    pub supervisor_id: Id,
}

/// ProcessRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessRef {
    /// `process_generation`; protocol §1.3.
    pub process_generation: U64,
    /// `process_identity`; protocol §1.3.
    pub process_identity: Knowledge<ProcessIdentity>,
    /// `connection_epoch`; protocol §1.3.
    pub connection_epoch: Id,
}

/// Correlation of a native RPC, blocking hook, or absent request; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum NativeRequestKey {
    /// RPC numbers retain their original decimal spelling and type.
    Rpc {
        /// Original JSON type.
        value_type: NativeRequestValueType,
        /// Native request value.
        value: String,
    },
    /// A local blocking hook invocation.
    Hook {
        /// Durable local invocation identity.
        invocation_id: Id,
    },
    /// This event has no native request.
    None,
}

/// AttachRef; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachRef {
    /// `native_ref`; protocol §3.1.
    pub native_ref: NativeRef,
    /// `process_ref`; protocol §3.1.
    pub process_ref: ProcessRef,
    /// `mode`; protocol §3.1.
    pub mode: AttachMode,
    /// `allow_wake`; protocol §3.1.
    pub allow_wake: BoolLiteral<false>,
}
