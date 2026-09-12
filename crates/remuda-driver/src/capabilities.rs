//! Driver capability matrix from `protocol.md` §3.3.

use crate::binary::BinaryPin;
use remuda_protocol::{
    AdapterTransport, Capability, CapabilityEvidence, CapabilityName, CapabilitySet,
    CapabilitySnapshot, CapabilityState, DriverKind, EvidenceType, Id, Knowledge, U64,
};

/// Adapter version stamped on capability snapshots.
pub const ADAPTER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Matrix cell: native evidence (`S*`), not provided (`N`), or unverified (`U`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixMark {
    /// Native evidence exists; adapter still needs listed conditions.
    SupportedStar,
    /// This driver v1 explicitly does not provide the capability.
    NotProvided,
    /// Evidence is insufficient; treat as unknown, never as false.
    Unknown,
}

/// Look up one cell of the §3.3 matrix (plus M0 `claude-bg`).
pub fn capability_matrix(kind: DriverKind, name: CapabilityName) -> MatrixMark {
    use CapabilityName::*;
    use DriverKind::*;
    match (kind, name) {
        (ClaudePrint, Resume | ModelSwitch | Fork | StructuredWorkflow | Hooks) => {
            MatrixMark::SupportedStar
        }
        (ClaudePrint, InteractiveApproval | Question | CompletionNativeTurn) => {
            MatrixMark::SupportedStar
        }
        (ClaudePrint, Artifact | TtyAttach | LiveAttach | PlanReview | Elicitation) => {
            if matches!(name, Artifact | TtyAttach | LiveAttach) {
                MatrixMark::NotProvided
            } else {
                MatrixMark::Unknown
            }
        }
        (ClaudePrint, Steer | CompletionTask) => MatrixMark::Unknown,
        (ClaudePty, Resume | Fork | StructuredWorkflow | Artifact | TtyAttach | Hooks) => {
            MatrixMark::SupportedStar
        }
        (ClaudePty, ModelSwitch | LiveAttach | CompletionNativeTurn) => MatrixMark::SupportedStar,
        (ClaudePty, Steer | InteractiveApproval | Question | PlanReview | Elicitation) => {
            if name == Steer {
                MatrixMark::NotProvided
            } else {
                MatrixMark::Unknown
            }
        }
        (ClaudePty, CompletionTask) => MatrixMark::Unknown,
        (ClaudeBg, Resume | StructuredWorkflow | Hooks | TtyAttach | LiveAttach) => {
            MatrixMark::SupportedStar
        }
        (ClaudeBg, CompletionNativeTurn) => MatrixMark::SupportedStar,
        (ClaudeBg, Steer | InteractiveApproval | Question) => MatrixMark::NotProvided,
        (ClaudeBg, _) => MatrixMark::Unknown,
        (CodexAppserver, Resume | Steer | ModelSwitch | Fork | Artifact | Hooks) => {
            MatrixMark::SupportedStar
        }
        (CodexAppserver, CompletionNativeTurn | InteractiveApproval | Question | Elicitation) => {
            MatrixMark::SupportedStar
        }
        (CodexAppserver, StructuredWorkflow | TtyAttach | LiveAttach) => MatrixMark::NotProvided,
        (CodexAppserver, _) => MatrixMark::Unknown,
        (GrokAcp, Resume | Artifact | Hooks | CompletionNativeTurn) => MatrixMark::SupportedStar,
        (GrokAcp, Steer | StructuredWorkflow | TtyAttach | LiveAttach | Fork) => {
            if matches!(name, Fork) {
                MatrixMark::Unknown
            } else {
                MatrixMark::NotProvided
            }
        }
        (GrokAcp, _) => MatrixMark::Unknown,
        (AgyPrint, Resume | CompletionNativeTurn) => MatrixMark::SupportedStar,
        (AgyPrint, Steer | ModelSwitch | StructuredWorkflow | TtyAttach | LiveAttach) => {
            MatrixMark::NotProvided
        }
        (AgyPrint, InteractiveApproval | Question | PlanReview | Elicitation | CompletionTask) => {
            MatrixMark::NotProvided
        }
        (AgyPrint, _) => MatrixMark::Unknown,
        (GenericPty, TtyAttach | LiveAttach) => MatrixMark::SupportedStar,
        (GenericPty, _) => MatrixMark::NotProvided,
        (ShellPty, TtyAttach | LiveAttach) => MatrixMark::SupportedStar,
        (ShellPty, _) => MatrixMark::NotProvided,
    }
}

/// Build a [`CapabilitySnapshot`] from the matrix and a binary pin.
pub fn capability_snapshot(
    kind: DriverKind,
    binary: &BinaryPin,
    settings_revision: U64,
    provider_profile_revision: U64,
) -> Result<CapabilitySnapshot, remuda_protocol::WireValueError> {
    Ok(CapabilitySnapshot {
        adapter_transport: adapter_transport(kind),
        id: Id::new("obj")?,
        driver_kind: kind,
        adapter_version: ADAPTER_VERSION.to_string(),
        binary_version: binary.version.clone(),
        binary_digest: binary.sha256.clone(),
        native_protocol_version: Knowledge::Unknown {
            reason: "not-negotiated".into(),
            evidence_event_ids: vec![],
        },
        settings_revision,
        provider_profile_revision,
        capabilities: capability_set(kind),
    })
}

/// Materialize the full [`CapabilitySet`] for `kind`.
pub fn capability_set(kind: DriverKind) -> CapabilitySet {
    CapabilitySet {
        resume: cap(kind, CapabilityName::Resume),
        steer: cap(kind, CapabilityName::Steer),
        model_switch: cap(kind, CapabilityName::ModelSwitch),
        fork: cap(kind, CapabilityName::Fork),
        structured_workflow: cap(kind, CapabilityName::StructuredWorkflow),
        artifact: cap(kind, CapabilityName::Artifact),
        tty_attach: cap(kind, CapabilityName::TtyAttach),
        hooks: cap(kind, CapabilityName::Hooks),
        interactive_approval: cap(kind, CapabilityName::InteractiveApproval),
        question: cap(kind, CapabilityName::Question),
        plan_review: cap(kind, CapabilityName::PlanReview),
        elicitation: cap(kind, CapabilityName::Elicitation),
        live_attach: cap(kind, CapabilityName::LiveAttach),
        completion_native_turn: cap(kind, CapabilityName::CompletionNativeTurn),
        completion_task: cap(kind, CapabilityName::CompletionTask),
    }
}

fn adapter_transport(kind: DriverKind) -> AdapterTransport {
    match kind {
        DriverKind::ClaudePrint => AdapterTransport::NativeRustWire,
        DriverKind::ClaudePty => AdapterTransport::ClaudePtyHerdr,
        DriverKind::ClaudeBg => AdapterTransport::ClaudeBgHerdrAttach,
        DriverKind::CodexAppserver => AdapterTransport::CodexAppserverSpawn,
        DriverKind::GrokAcp => AdapterTransport::GrokAcp,
        DriverKind::AgyPrint => AdapterTransport::AgyNative,
        DriverKind::GenericPty => AdapterTransport::GenericHerdr,
        DriverKind::ShellPty => AdapterTransport::ShellPty,
    }
}

fn cap(kind: DriverKind, name: CapabilityName) -> Capability {
    let mark = capability_matrix(kind, name);
    let (state, reason_code) = match mark {
        MatrixMark::SupportedStar => (CapabilityState::Supported, "native-evidence"),
        MatrixMark::NotProvided => (CapabilityState::Unsupported, "not-provided"),
        MatrixMark::Unknown => (CapabilityState::Unknown, "insufficient-evidence"),
    };
    let evidence = if mark == MatrixMark::SupportedStar {
        vec![CapabilityEvidence {
            actor_type: EvidenceType::Source,
            reference: "docs/design/protocol.md#3.3".into(),
            digest: Knowledge::Unknown {
                reason: "not-hashed".into(),
                evidence_event_ids: vec![],
            },
        }]
    } else {
        vec![]
    };
    Capability {
        state,
        scope: vec![],
        reason_code: reason_code.into(),
        prerequisites: vec![],
        evidence,
    }
}
