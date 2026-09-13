//! Native wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};

/// TranscriptRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptRef {
    /// `object_id`; protocol §1.3.
    pub object_id: Id,
    /// `source_path`; protocol §1.3.
    pub source_path: String,
}

/// CodexRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodexRef {
    /// `thread_id`; protocol §1.3.
    pub thread_id: String,
}

/// AcpRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcpRef {
    /// `session_id`; protocol §1.3.
    pub session_id: String,
    /// `protocol_version`; protocol §1.3.
    pub protocol_version: u32,
}

/// ClaudeRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeRef {
    /// `session_id`; protocol §1.3.
    pub session_id: String,
}

/// AgyRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgyRef {
    /// `conversation_id`; protocol §1.3.
    pub conversation_id: String,
}

/// Pinned Herdr binary and current server lifetime; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HerdrServer {
    /// Absolute path selected by the Node.
    pub binary_path: String,
    /// Reported Herdr binary version.
    pub version: String,
    /// SHA-256 of the selected executable.
    pub digest: Digest,
    /// Negotiated native socket protocol version, independent of Remuda's version.
    pub protocol_version: String,
    /// Durable identity of the registered Herdr server.
    pub server_identity: Id,
    /// Changes whenever the server process is replaced.
    pub server_epoch: Id,
    /// Herdr exposes rendered terminal frames, not original PTY bytes.
    pub representation: HerdrRepresentation,
}

/// Herdr pane identity within a named server session; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HerdrRef {
    /// Binary pin and server lifetime attached to this observation.
    #[serde(flatten)]
    pub server: HerdrServer,
    /// Explicit Herdr session name; never inferred from inherited environment.
    pub session: String,
    /// Native pane ID; not a Claude session or background job ID.
    pub pane_id: String,
}

/// A Claude daemon job that may exist before its native session is known; §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeBgRef {
    /// Full native job ID, scoped by NativeRef's host and native store.
    pub job_id: String,
}

/// One capability this live session actually reached, with the tier that
/// proves it; `protocol.md` §1.3 (D-028 §4.3).
///
/// A runtime entry outranks the static `DriverKind` matrix for the same name.
/// It carries its own `state`, so a session may report a capability as
/// `unknown` just as truthfully as `supported`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCapability {
    /// Which capability this entry overrides.
    pub name: CapabilityName,
    /// Observed state. `unknown` is honest and stays callable-refusing.
    pub state: CapabilityState,
    /// Signal tier that produced the observation.
    pub tier: SignalTier,
    /// Stable machine-readable cause (`hook-socket-open`, `no-adapter`, …).
    pub reason_code: String,
}

/// NativeRef; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Highest signal layer this session actually reached; §1.3 (D-028 §4.3).
    ///
    /// Absent on pre-D-028 payloads and on drivers that never report one.
    /// Absent is not [`SignalTier::None`]: it means "nobody said", so
    /// [`crate::CapabilitySet`] falls back to the static driver matrix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_tier: Option<SignalTier>,
    /// Capabilities this session reports at runtime; §1.3 (D-028 §4.3).
    ///
    /// Empty or absent means "no runtime report"; the static matrix stands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<RuntimeCapability>,
    /// `codex`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexRef>,
    /// `acp`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpRef>,
    /// `claude`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeRef>,
    /// Background job identity independent of the native conversation identity; §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_bg: Option<ClaudeBgRef>,
    /// `agy`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agy: Option<AgyRef>,
    /// `herdr`; protocol §1.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr: Option<HerdrRef>,
}

/// ProcessIdentity; `protocol.md` §1.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
