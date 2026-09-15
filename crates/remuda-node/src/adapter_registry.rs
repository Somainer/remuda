//! Node-side registry of the per-harness file signal adapters (D-028 §4.3, P6).
//!
//! The driver owns the adapter state machines and their file tails; this file
//! is the deliberately small registry the Node consults when it needs to know
//! *which structured channels a session actually has*, without reading the
//! driver internals:
//!
//! * the [`SignalTier`] the session should report once its hook+file channels
//!   are live (the §4.3 tier feeds the runtime capability override);
//! * whether a harness answers approvals through a hook at all, and if so
//!   whether the answer is a typed verdict or screen-emulated
//!   (`interactive-approval` for grok).
//!
//! Kept dependency-free and in its own file so the shared Node signal/run-time
//! paths do not grow per-kind branches.

use remuda_protocol::{AgentKind, SignalTier};

/// Structured-signal capabilities of one harness's adapter.
///
/// Consumed by the runtime capability fold and the interaction routing; the
/// variants document the §3 signal matrix even where a later phase reads
/// them, so they are not pruned on a "currently unused" basis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct AdapterInfo {
    /// Highest signal tier the adapter delivers once hooks are configured.
    ///
    /// Codex and grok both run hooks *and* structured file tails, so they are
    /// tier A `Hook`; the file channel is the content/message path, the hook
    /// channel the lifecycle/approval path.
    pub tier: SignalTier,
    /// How tool approvals are decided.
    pub approval: ApprovalChannel,
}

/// Where an approval verdict comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ApprovalChannel {
    /// Blocking hook returning a typed verdict (codex `PermissionRequest`).
    TypedHookVerdict,
    /// No hook verdict; the human answers on the screen (grok: PreToolUse can
    /// only deny/ask, so the real choice is a tier-D screen answer).
    EmulatedScreen,
    /// No structured approval channel at all.
    None,
}

/// Per-kind registry, the §3 signal matrix in lookup form.
#[must_use]
pub fn adapter_for(kind: AgentKind) -> Option<AdapterInfo> {
    match kind {
        AgentKind::Codex => Some(AdapterInfo {
            tier: SignalTier::Hook,
            approval: ApprovalChannel::TypedHookVerdict,
        }),
        AgentKind::Grok => Some(AdapterInfo {
            tier: SignalTier::Hook,
            approval: ApprovalChannel::EmulatedScreen,
        }),
        AgentKind::Claude => Some(AdapterInfo {
            tier: SignalTier::Hook,
            approval: ApprovalChannel::TypedHookVerdict,
        }),
        _ => None,
    }
}

/// Whether this kind has a file-tail adapter the driver should spawn.
#[must_use]
pub fn has_file_adapter(kind: AgentKind) -> bool {
    adapter_for(kind).is_some_and(|info| info.tier == SignalTier::Hook)
        && matches!(kind, AgentKind::Codex | AgentKind::Grok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_and_grok_have_file_adapters_and_hook_tier() {
        assert!(has_file_adapter(AgentKind::Codex));
        assert!(has_file_adapter(AgentKind::Grok));
        assert!(!has_file_adapter(AgentKind::Claude));
        assert!(!has_file_adapter(AgentKind::Terminal));
        for kind in [AgentKind::Codex, AgentKind::Grok, AgentKind::Claude] {
            let info = adapter_for(kind).expect("adapter info");
            assert_eq!(info.tier, SignalTier::Hook);
        }
        // The measured §6 approval split: codex answers through a typed hook
        // verdict; grok's real answer is a screen-emulated interaction.
        assert_eq!(
            adapter_for(AgentKind::Codex).unwrap().approval,
            ApprovalChannel::TypedHookVerdict
        );
        assert_eq!(
            adapter_for(AgentKind::Grok).unwrap().approval,
            ApprovalChannel::EmulatedScreen
        );
        assert!(adapter_for(AgentKind::Terminal).is_none());
    }
}
