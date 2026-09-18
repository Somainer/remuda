//! Driver capability matrix from `protocol.md` §3.3.

use crate::binary::BinaryPin;
use remuda_protocol::{
    AdapterTransport, Capability, CapabilityEvidence, CapabilityName, CapabilityProvision,
    CapabilitySet, CapabilitySnapshot, CapabilityState, DriverKind, EvidenceType, Id, Knowledge,
    NativeRef, SignalTier, U64,
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
    //
    // `claude-sdk` does not get an exception. It *does* route `instance.cancel`
    // to the native `control_request`/`interrupt` (`print-replacement.md` §2.3),
    // but the only witness in CI is `fake-claude`, which acks the frame because
    // its script says to — that proves Remuda writes the request, not that the
    // real CLI aborts a turn. Wiring a control request is not a measurement, so
    // the cell stays `unknown` until a live capture shows a turn cut short (D-037).
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
        // `claude-sdk` (`print-replacement.md` §2.6). Same native evidence as
        // print for the structured capabilities, plus multi-turn stdin for
        // resume. TTY is structurally absent: stdio is not a PTY, so this
        // carrier has no Terminal view to attach to (§1.11, §2.6).
        (ClaudeSdk, Resume | ModelSwitch | Fork | StructuredWorkflow | Hooks) => {
            MatrixMark::SupportedStar
        }
        (ClaudeSdk, InteractiveApproval | Question | CompletionNativeTurn) => {
            MatrixMark::SupportedStar
        }
        (ClaudeSdk, Artifact | TtyAttach | LiveAttach) => MatrixMark::NotProvided,
        // Steer stays unknown until measured on *this* transport: a second
        // `user` frame mid-turn may be native queue or steer, and we do not
        // claim which (§2.3, D-028a item 2). Queue and Interrupt are forced
        // Unknown above for every driver.
        (ClaudeSdk, _) => MatrixMark::Unknown,
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
            provision: entry.provision,
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
        // The tier *is* the harness's own signal channel, so what it grants is
        // native by construction. Anything Remuda stands in for is reported by
        // an explicit entry, which carries its own provision.
        provision: CapabilityProvision::Native,
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
        // D-003 reserved `ClaudeSdkSidecar` for a Node sidecar speaking the
        // published SDK; M1 writes the same NDJSON from Rust, so the transport
        // is the hand-written wire (`print-replacement.md` §2, decision 2).
        DriverKind::ClaudeSdk => AdapterTransport::NativeRustWire,
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
        // The static matrix records native evidence only (§3.3); nothing in it
        // describes a Remuda-emulated path, so a matrix cell never claims one.
        // §6's native/emulated distinction arrives with the runtime report.
        provision: match mark {
            MatrixMark::SupportedStar => CapabilityProvision::Native,
            MatrixMark::NotProvided | MatrixMark::Unknown => CapabilityProvision::Unknown,
        },
        scope: vec![],
        reason_code: reason_code.into(),
        prerequisites: vec![],
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{AgentKind, CapabilityProvision, HostId, RuntimeCapability};

    fn native_ref(tier: Option<SignalTier>, entries: Vec<RuntimeCapability>) -> NativeRef {
        NativeRef {
            host_id: HostId::new(),
            native_store_id: Id::new("obj").unwrap(),
            kind: AgentKind::Claude,
            session_id: Knowledge::Unknown {
                reason: "test".into(),
                evidence_event_ids: vec![],
            },
            transcript: Knowledge::Unknown {
                reason: "test".into(),
                evidence_event_ids: vec![],
            },
            signal_tier: tier,
            capabilities: entries,
            codex: None,
            acp: None,
            claude: None,
            claude_bg: None,
            agy: None,
            herdr: None,
        }
    }

    fn entry(name: CapabilityName, state: CapabilityState) -> RuntimeCapability {
        RuntimeCapability {
            name,
            state,
            provision: CapabilityProvision::Native,
            tier: SignalTier::Hook,
            reason_code: "measured".into(),
        }
    }

    /// D-028 §4.3: with nothing reported, the static matrix is unchanged. This
    /// is the fallback the whole override path rests on.
    #[test]
    fn no_runtime_report_leaves_the_static_matrix_alone() {
        for kind in [DriverKind::ShellPty, DriverKind::ClaudePrint] {
            assert_eq!(
                capability_set_with_runtime(kind, None),
                capability_set(kind),
                "{kind:?}"
            );
            // An empty report is the same as no report: a NativeRef that
            // simply has not been filled in yet must not look like a session
            // claiming it reached no signal tier at all.
            assert_eq!(
                capability_set_with_runtime(kind, Some(&native_ref(None, vec![]))),
                capability_set(kind),
                "{kind:?} empty"
            );
        }
    }

    /// The structural point of the whole change: a promoted shell-pty session
    /// can report capabilities its DriverKind says it does not have.
    #[test]
    fn a_signal_tier_lifts_shell_pty_above_its_static_row() {
        let stat = capability_set(DriverKind::ShellPty);
        assert_eq!(stat.hooks.state, CapabilityState::Unsupported);
        assert_eq!(stat.resume.state, CapabilityState::Unsupported);

        let hooked = capability_set_with_runtime(
            DriverKind::ShellPty,
            Some(&native_ref(Some(SignalTier::Hook), vec![])),
        );
        assert_eq!(hooked.hooks.state, CapabilityState::Supported);
        assert_eq!(hooked.resume.state, CapabilityState::Supported);
        assert_eq!(
            hooked.interactive_approval.state,
            CapabilityState::Supported
        );
        assert_eq!(hooked.hooks.reason_code, "signal-tier-hook");
        // tty-attach was already supported and stays so: the tier adds, it
        // does not reset the row.
        assert_eq!(hooked.tty_attach.state, CapabilityState::Supported);

        // File tail proves identity and turn boundaries but cannot block the
        // agent for a verdict, so it must not claim interactive-approval.
        let tailed = capability_set_with_runtime(
            DriverKind::ShellPty,
            Some(&native_ref(Some(SignalTier::File), vec![])),
        );
        assert_eq!(tailed.resume.state, CapabilityState::Supported);
        assert_eq!(
            tailed.interactive_approval.state,
            CapabilityState::Unsupported
        );
        assert_eq!(tailed.hooks.state, CapabilityState::Unsupported);

        // Screen and OSC prove neither; they are the floor for a reason.
        for tier in [SignalTier::Osc, SignalTier::Screen, SignalTier::None] {
            let floor = capability_set_with_runtime(
                DriverKind::ShellPty,
                Some(&native_ref(Some(tier), vec![])),
            );
            assert_eq!(floor, capability_set(DriverKind::ShellPty), "{tier:?}");
        }
    }

    /// An explicit entry beats the tier default, in **both** directions. A
    /// session that measured a capability as absent must be able to say so,
    /// or "runtime override" would only ever be able to add optimism.
    #[test]
    fn explicit_entries_win_over_the_tier_and_may_lower_a_capability() {
        let downgraded = capability_set_with_runtime(
            DriverKind::ShellPty,
            Some(&native_ref(
                Some(SignalTier::Hook),
                vec![entry(
                    CapabilityName::InteractiveApproval,
                    CapabilityState::Unsupported,
                )],
            )),
        );
        assert_eq!(
            downgraded.interactive_approval.state,
            CapabilityState::Unsupported
        );
        // Its neighbours from the same tier are untouched.
        assert_eq!(downgraded.hooks.state, CapabilityState::Supported);

        // And an entry can override a statically-supported cell downwards.
        let print = capability_set_with_runtime(
            DriverKind::ClaudePrint,
            Some(&native_ref(
                None,
                vec![entry(CapabilityName::Resume, CapabilityState::Unknown)],
            )),
        );
        assert_eq!(
            capability_set(DriverKind::ClaudePrint).resume.state,
            CapabilityState::Supported
        );
        assert_eq!(print.resume.state, CapabilityState::Unknown);
        assert_eq!(print.resume.reason_code, "measured");

        // §6: a capability Remuda stands in for reports `emulated`, and the
        // runtime entry carries that through. The tier's own grants stay
        // `native` — the tier *is* the harness's channel.
        let emulated = capability_set_with_runtime(
            DriverKind::ShellPty,
            Some(&native_ref(
                Some(SignalTier::Hook),
                vec![RuntimeCapability {
                    name: CapabilityName::Queue,
                    state: CapabilityState::Supported,
                    provision: CapabilityProvision::Emulated,
                    tier: SignalTier::Hook,
                    reason_code: "remuda-ledger".into(),
                }],
            )),
        );
        assert_eq!(emulated.queue.provision, CapabilityProvision::Emulated);
        assert_eq!(emulated.hooks.provision, CapabilityProvision::Native);
        // The static matrix never claims a provider: it records native
        // evidence for `supported` cells and says nothing for the rest.
        let stat = capability_set(DriverKind::ShellPty);
        assert_eq!(stat.tty_attach.provision, CapabilityProvision::Native);
        assert_eq!(stat.queue.provision, CapabilityProvision::Unknown);
        assert_eq!(stat.resume.provision, CapabilityProvision::Unknown);
    }

    /// `print-replacement.md` §2.6: the honest `claude-sdk` row. No TTY on this
    /// carrier, structured capabilities carried over from print, and steer left
    /// unknown because it has not been measured on this transport.
    #[test]
    fn the_sdk_row_reports_no_tty_and_an_unmeasured_steer() {
        let set = capability_set(DriverKind::ClaudeSdk);

        // Stdio is not a PTY: there is no Terminal view to attach to, and the UI
        // must not offer one. `unsupported` is a fact here, not a guess.
        assert_eq!(set.tty_attach.state, CapabilityState::Unsupported);
        assert_eq!(set.live_attach.state, CapabilityState::Unsupported);
        assert_eq!(set.tty_attach.reason_code, "not-provided");

        // Same native evidence as print, plus multi-turn stdin for resume.
        for cap in [
            &set.resume,
            &set.interactive_approval,
            &set.question,
            &set.completion_native_turn,
        ] {
            assert_eq!(cap.state, CapabilityState::Supported);
            assert_eq!(cap.provision, CapabilityProvision::Native);
        }

        // A second `user` frame mid-turn may be native queue or steer; we do not
        // claim which until it is measured (D-028a item 2).
        assert_eq!(set.steer.state, CapabilityState::Unknown);
        assert_eq!(set.steer.reason_code, "insufficient-evidence");

        // The transport is the hand-written Rust wire, not the reserved sidecar.
        assert_eq!(
            adapter_transport(DriverKind::ClaudeSdk),
            AdapterTransport::NativeRustWire
        );
    }

    /// The signal-tier floor: this carrier has no hook/file/OSC/screen ladder, so
    /// `None` must add nothing — stdout is not `Hook` (§2.5).
    #[test]
    fn signal_tier_none_grants_the_sdk_carrier_nothing() {
        assert!(tier_capabilities(SignalTier::None).is_empty());
        assert_eq!(
            capability_set_with_runtime(
                DriverKind::ClaudeSdk,
                Some(&native_ref(Some(SignalTier::None), vec![]))
            ),
            capability_set(DriverKind::ClaudeSdk)
        );
    }

    /// §6: no driver claims queue/interrupt in this task. They are unmeasured
    /// (§14 risks 6 and 7), and `unknown` is what unmeasured means.
    #[test]
    fn queue_and_interrupt_are_unknown_for_every_driver() {
        for kind in [
            DriverKind::ClaudePrint,
            DriverKind::ClaudeSdk,
            DriverKind::ClaudePty,
            DriverKind::ClaudeBg,
            DriverKind::CodexAppserver,
            DriverKind::GrokAcp,
            DriverKind::AgyPrint,
            DriverKind::GenericPty,
            DriverKind::ShellPty,
        ] {
            let set = capability_set(kind);
            assert_eq!(set.queue.state, CapabilityState::Unknown, "{kind:?} queue");
            assert_eq!(
                set.interrupt.state,
                CapabilityState::Unknown,
                "{kind:?} interrupt"
            );
        }
        // steer on a PTY carrier is unknown too: whether typing into a busy
        // agent TUI steers or queues has not been measured on any harness.
        assert_eq!(
            capability_set(DriverKind::ShellPty).steer.state,
            CapabilityState::Unknown
        );
    }
}
