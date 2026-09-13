//! Driver capability matrix from `protocol.md` §3.3.

use crate::binary::BinaryPin;
use remuda_protocol::{
    AdapterTransport, Capability, CapabilityEvidence, CapabilityName, CapabilitySet,
    CapabilitySnapshot, CapabilityState, DriverKind, EvidenceType, Id, Knowledge, NativeRef,
    SignalTier, U64,
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
    // D-028 §6: `queue` and `interrupt` are unverified for every driver in
    // this task — no driver behaviour changed here, and §14 risk 6/7 record
    // that claude's queue-vs-steer semantics and grok/agy's keys are still
    // unmeasured. `unknown` is the truthful answer for all of them; the
    // per-harness measurements land with the worker who implements the keys.
    if matches!(name, Queue | Interrupt) {
        return MatrixMark::Unknown;
    }
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
        (ClaudePrint, Steer | CompletionTask | Queue | Interrupt) => MatrixMark::Unknown,
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
        (ClaudePty, CompletionTask | Queue | Interrupt) => MatrixMark::Unknown,
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
        (GenericPty, Steer) => MatrixMark::Unknown,
        (GenericPty, _) => MatrixMark::NotProvided,
        (ShellPty, TtyAttach | LiveAttach) => MatrixMark::SupportedStar,
        // D-028 §6 / §14 risk 6: whether typing into a busy agent TUI steers
        // or queues is unmeasured for every harness this carrier can host.
        // `unsupported` would be a claim; `unknown` is what we know.
        (ShellPty, Steer) => MatrixMark::Unknown,
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

/// Materialize the full [`CapabilitySet`] for `kind` from the static matrix.
pub fn capability_set(kind: DriverKind) -> CapabilitySet {
    capability_set_with_runtime(kind, None)
}

/// [`capability_set`], with any runtime report from `native_ref` layered on top.
///
/// D-028 §4.3: the static matrix keys off [`DriverKind`] alone, so a promoted
/// `shell-pty` session could never report the capabilities it actually gained
/// by being an agent. Runtime values win when present; the matrix is the
/// fallback, not the authority.
///
/// Two rules keep this honest:
///
/// * a runtime entry replaces the matrix cell **whatever its state** — a
///   session that observes `steer` is unavailable says so, rather than
///   inheriting a matrix `supported`;
/// * a capability with no runtime entry keeps its matrix value, because
///   "not reported" is not evidence of absence.
pub fn capability_set_with_runtime(
    kind: DriverKind,
    native_ref: Option<&NativeRef>,
) -> CapabilitySet {
    let mut set = CapabilitySet {
        resume: cap(kind, CapabilityName::Resume),
        steer: cap(kind, CapabilityName::Steer),
        queue: cap(kind, CapabilityName::Queue),
        interrupt: cap(kind, CapabilityName::Interrupt),
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
    };
    let Some(native_ref) = native_ref else {
        return set;
    };
    // A declared tier with no explicit entries still says something: the tier
    // itself is evidence for the capabilities it structurally provides.
    if let Some(tier) = native_ref.signal_tier {
        for name in tier_capabilities(tier) {
            *slot(&mut set, *name) = tier_cap(tier);
        }
    }
    // Explicit entries are more specific than the tier default, so they are
    // applied second and win.
    for entry in &native_ref.capabilities {
        *slot(&mut set, entry.name) = Capability {
            state: entry.state,
            scope: vec![],
            reason_code: entry.reason_code.clone(),
            prerequisites: vec![],
            evidence: vec![runtime_evidence(entry.tier)],
        };
    }
    set
}

/// Capabilities a signal tier structurally provides; D-028 §4.3.
///
/// A hook socket is the only tier that can block an agent and return a verdict,
/// which is what `interactive-approval` means; it also implies the harness is
/// emitting hook events at all, and that a session id was reported (so
/// `resume`) and turns are delimited (`completion-native-turn`). File tail
/// gives the same identity and turn boundaries without the blocking channel.
/// OSC and Screen prove neither, so they add nothing here — that is the point
/// of ranking them lowest.
fn tier_capabilities(tier: SignalTier) -> &'static [CapabilityName] {
    use CapabilityName::*;
    match tier {
        SignalTier::Hook => &[
            Resume,
            Hooks,
            StructuredWorkflow,
            CompletionNativeTurn,
            InteractiveApproval,
        ],
        SignalTier::File => &[Resume, StructuredWorkflow, CompletionNativeTurn],
        SignalTier::Osc | SignalTier::Screen | SignalTier::None => &[],
    }
}

fn tier_cap(tier: SignalTier) -> Capability {
    Capability {
        state: CapabilityState::Supported,
        scope: vec![],
        reason_code: format!("signal-tier-{}", tier_slug(tier)),
        prerequisites: vec![],
        evidence: vec![runtime_evidence(tier)],
    }
}

fn tier_slug(tier: SignalTier) -> &'static str {
    match tier {
        SignalTier::Hook => "hook",
        SignalTier::File => "file",
        SignalTier::Osc => "osc",
        SignalTier::Screen => "screen",
        SignalTier::None => "none",
    }
}

fn runtime_evidence(tier: SignalTier) -> CapabilityEvidence {
    CapabilityEvidence {
        actor_type: EvidenceType::NativeNegotiation,
        reference: format!("runtime:signal-tier:{}", tier_slug(tier)),
        digest: Knowledge::Unknown {
            reason: "not-hashed".into(),
            evidence_event_ids: vec![],
        },
    }
}

fn slot(set: &mut CapabilitySet, name: CapabilityName) -> &mut Capability {
    match name {
        CapabilityName::Resume => &mut set.resume,
        CapabilityName::Steer => &mut set.steer,
        CapabilityName::Queue => &mut set.queue,
        CapabilityName::Interrupt => &mut set.interrupt,
        CapabilityName::ModelSwitch => &mut set.model_switch,
        CapabilityName::Fork => &mut set.fork,
        CapabilityName::StructuredWorkflow => &mut set.structured_workflow,
        CapabilityName::Artifact => &mut set.artifact,
        CapabilityName::TtyAttach => &mut set.tty_attach,
        CapabilityName::Hooks => &mut set.hooks,
        CapabilityName::InteractiveApproval => &mut set.interactive_approval,
        CapabilityName::Question => &mut set.question,
        CapabilityName::PlanReview => &mut set.plan_review,
        CapabilityName::Elicitation => &mut set.elicitation,
        CapabilityName::LiveAttach => &mut set.live_attach,
        CapabilityName::CompletionNativeTurn => &mut set.completion_native_turn,
        CapabilityName::CompletionTask => &mut set.completion_task,
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
