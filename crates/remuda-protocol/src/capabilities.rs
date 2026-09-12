//! Capabilities wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};

/// CapabilityEvidence; `protocol.md` §3.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityEvidence {
    /// `actor_type`; protocol §3.2.
    #[serde(rename = "type")]
    pub actor_type: EvidenceType,
    /// `reference`; protocol §3.2.
    #[serde(rename = "ref")]
    pub reference: String,
    /// `digest`; protocol §3.2.
    pub digest: Knowledge<Digest>,
}

/// Capability; `protocol.md` §3.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capability {
    /// `state`; protocol §3.2.
    pub state: CapabilityState,
    /// `scope`; protocol §3.2.
    pub scope: Vec<String>,
    /// `reason_code`; protocol §3.2.
    pub reason_code: String,
    /// `prerequisites`; protocol §3.2.
    pub prerequisites: Vec<String>,
    /// `evidence`; protocol §3.2.
    pub evidence: Vec<CapabilityEvidence>,
}

/// Complete capability record; `protocol.md` §3.2. Missing capabilities are invalid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CapabilitySet {
    /// Capability `resume`; §3.2.
    pub resume: Capability,
    /// Capability `steer`; §3.2.
    pub steer: Capability,
    /// Capability `model-switch`; §3.2.
    pub model_switch: Capability,
    /// Capability `fork`; §3.2.
    pub fork: Capability,
    /// Capability `structured-workflow`; §3.2.
    pub structured_workflow: Capability,
    /// Capability `artifact`; §3.2.
    pub artifact: Capability,
    /// Capability `tty-attach`; §3.2.
    pub tty_attach: Capability,
    /// Capability `hooks`; §3.2.
    pub hooks: Capability,
    /// Capability `interactive-approval`; §3.2.
    pub interactive_approval: Capability,
    /// Capability `question`; §3.2.
    pub question: Capability,
    /// Capability `plan-review`; §3.2.
    pub plan_review: Capability,
    /// Capability `elicitation`; §3.2.
    pub elicitation: Capability,
    /// Capability `live-attach`; §3.2.
    pub live_attach: Capability,
    /// Capability `completion-native-turn`; §3.2.
    pub completion_native_turn: Capability,
    /// Capability `completion-task`; §3.2.
    pub completion_task: Capability,
}

/// CapabilitySnapshot; `protocol.md` §3.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySnapshot {
    /// `id`; protocol §3.2.
    pub id: Id,
    /// `driver_kind`; protocol §3.2.
    pub driver_kind: DriverKind,
    /// `adapter_version`; protocol §3.2.
    pub adapter_version: String,
    /// `binary_version`; protocol §3.2.
    pub binary_version: String,
    /// `binary_digest`; protocol §3.2.
    pub binary_digest: Digest,
    /// `native_protocol_version`; protocol §3.2.
    pub native_protocol_version: Knowledge<String>,
    /// `settings_revision`; protocol §3.2.
    pub settings_revision: U64,
    /// `provider_profile_revision`; protocol §3.2.
    pub provider_profile_revision: U64,
    /// `capabilities`; protocol §3.2.
    pub capabilities: CapabilitySet,
}

/// DriverDescriptor; `protocol.md` §3.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverDescriptor {
    /// `kind`; protocol §3.2.
    pub kind: DriverKind,
    /// `adapter_version`; protocol §3.2.
    pub adapter_version: String,
    /// `binary_path`; protocol §3.2.
    pub binary_path: String,
    /// `binary_version`; protocol §3.2.
    pub binary_version: String,
    /// `binary_digest`; protocol §3.2.
    pub binary_digest: Digest,
    /// `launchable`; protocol §3.2.
    pub launchable: bool,
    /// `reason_code`; protocol §3.2.
    pub reason_code: String,
    /// `capabilities`; protocol §3.2.
    pub capabilities: CapabilitySnapshot,
}
