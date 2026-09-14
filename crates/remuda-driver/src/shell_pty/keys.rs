//! Per-harness key semantics for steer, queue and interrupt (D-028 §6).
//!
//! The three composer actions do not map onto one key each. Every harness
//! measured in this round disagrees with the others about what Enter means
//! mid-turn, and two of them disagree about whether `Esc` interrupts at all.
//! This module is the single place those measurements live, so that no caller
//! has to remember that `Esc` is Claude's interrupt but Grok's no-op.
//!
//! Evidence: [claude-queue-steer-1], [codex-signals-1], [grok-signals-1].
//! Anything not measured is [`Provision::Unknown`] and stays that way —
//! §6's honesty rule makes "we have not tested this" a reportable answer
//! rather than a silent `false`.
//!
//! [claude-queue-steer-1]: ../../../docs/design/evidence/claude-queue-steer-1.md
//! [codex-signals-1]: ../../../docs/design/evidence/codex-signals-1.md
//! [grok-signals-1]: ../../../docs/design/evidence/grok-signals-1.md

use remuda_protocol::{AgentKind, CapabilityProvision};

/// Who implements a composer action for a given harness.
///
/// Mirrors [`CapabilityProvision`]; kept as its own type so the key table can
/// be read and tested without a protocol value in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provision {
    /// The harness does it itself, with its own key.
    Native,
    /// Remuda does it, by holding state or by sending a sequence the harness
    /// never designed for this purpose. §6 requires this be visible in the UI.
    Emulated,
    /// Not measured for this harness. Never silently downgraded to
    /// "unsupported": that would be a claim we have not earned.
    Unknown,
}

impl Provision {
    /// Protocol value for this provision.
    #[must_use]
    pub fn wire(self) -> CapabilityProvision {
        match self {
            Self::Native => CapabilityProvision::Native,
            Self::Emulated => CapabilityProvision::Emulated,
            Self::Unknown => CapabilityProvision::Unknown,
        }
    }
}

/// What an interrupt costs and how it is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupt {
    /// Logical keys, in order, that end the current turn.
    pub keys: &'static [&'static str],
    /// Who is implementing it.
    pub provision: Provision,
}

/// The measured composer semantics of one harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessKeys {
    /// Kind this row describes.
    pub kind: AgentKind,
    /// How "send now" behaves.
    ///
    /// `Native` does **not** promise immediacy — Claude's Enter is a native
    /// enqueue consumed at the next tool boundary, which §6 insists be
    /// described as such rather than as instant steering.
    pub steer: Provision,
    /// Whether the harness has an after-this-turn queue Remuda can address.
    ///
    /// `Emulated` means Remuda holds the text and delivers it on the harness's
    /// own turn-end signal; only codex has a native key (`Tab`).
    pub queue: Provision,
    /// The native key that queues, when there is one.
    pub queue_key: Option<&'static str>,
    /// How to end the running turn.
    pub interrupt: Interrupt,
    /// The harness has no way to deliver a message *now* without destroying
    /// the running turn.
    ///
    /// Grok's only send-now transport is "cancel the turn, then the queued item
    /// goes out". §6 forbids implementing the plain send button that way, so
    /// this flag makes the caller ask first.
    pub send_now_costs_turn: bool,
}

/// Turn-end signal each harness emits, consumed by the emulated queue (§6
/// mapping rule 2). Documented here beside the keys so the two halves of the
/// emulated queue cannot drift apart.
#[must_use]
pub fn turn_end_signal(kind: AgentKind) -> Option<&'static str> {
    match kind {
        AgentKind::Claude => Some("Stop"),
        AgentKind::Codex => Some("task_complete"),
        AgentKind::Grok => Some("turn_ended"),
        _ => None,
    }
}

/// Measured semantics, one row per harness. §6's table, in code.
pub const HARNESS_KEYS: &[HarnessKeys] = &[
    HarnessKeys {
        kind: AgentKind::Claude,
        // [V] Enter enqueues natively and the item is absorbed at the next
        // tool boundary (`queue-operation{enqueue}` → `remove{absorbed_mid_turn}`).
        steer: Provision::Native,
        // [V] The native queue has no after-turn-only guarantee: both Enter and
        // `Ctrl+X`+Enter were absorbed mid-turn. Remuda therefore holds the
        // text itself and delivers it on `Stop`.
        queue: Provision::Emulated,
        queue_key: None,
        // [V] `Esc` interrupts a running tool and leaves the process alive; the
        // native queue survives it.
        interrupt: Interrupt {
            keys: &["esc"],
            provision: Provision::Native,
        },
        send_now_costs_turn: false,
    },
    HarnessKeys {
        kind: AgentKind::Codex,
        // [V] Enter joins the *current* turn (same `turn_id`).
        steer: Provision::Native,
        // [V] `Tab` is a real next-turn queue. The one native queue in the set.
        queue: Provision::Native,
        queue_key: Some("tab"),
        // [V] `Esc` → `turn_aborted{reason:"interrupted"}`. Note this aborts the
        // turn, not necessarily a tool process already running.
        interrupt: Interrupt {
            keys: &["esc"],
            provision: Provision::Native,
        },
        send_now_costs_turn: false,
    },
    HarnessKeys {
        kind: AgentKind::Grok,
        // [V] No send-now transport exists. Enter queues; the only way to make
        // a queued item go out immediately is to cancel the running turn.
        steer: Provision::Emulated,
        // [V] Enter does queue, but there is no enqueue record and no way to
        // amend the harness's ledger, so Remuda keeps its own.
        queue: Provision::Emulated,
        queue_key: None,
        // [V] `Esc` is *not* the interrupt: it keeps the turn and the draft and
        // only prints a hint. The first `Ctrl+C` clears the draft, the second
        // cancels — so one `Ctrl+C` is not an interrupt receipt.
        interrupt: Interrupt {
            keys: &["ctrl+c", "ctrl+c"],
            provision: Provision::Emulated,
        },
        send_now_costs_turn: true,
    },
    HarnessKeys {
        kind: AgentKind::Agy,
        // Nothing measured. §6: show "not yet verified", never a grey button
        // pretending the answer is no.
        steer: Provision::Unknown,
        queue: Provision::Unknown,
        queue_key: None,
        interrupt: Interrupt {
            keys: &[],
            provision: Provision::Unknown,
        },
        send_now_costs_turn: false,
    },
];

/// Measured semantics for `kind`, when it is an agent we have a row for.
#[must_use]
pub fn keys_for(kind: AgentKind) -> Option<&'static HarnessKeys> {
    HARNESS_KEYS.iter().find(|row| row.kind == kind)
}

/// Bytes that interrupt the current turn of `kind`.
///
/// `None` when the harness has no measured interrupt (agy): §5.3 would rather
/// return `CAPABILITY_UNKNOWN` than send a plausible-looking key and report
/// success. A plain shell — no promoted kind — is not this function's business;
/// its caller sends `Ctrl+C`, which for a shell genuinely is the interrupt.
#[must_use]
pub fn interrupt_bytes(kind: AgentKind) -> Option<Vec<u8>> {
    let row = keys_for(kind)?;
    if row.interrupt.keys.is_empty() {
        return None;
    }
    Some(crate::tty::logical_keys_to_bytes(
        &row.interrupt
            .keys
            .iter()
            .map(|key| (*key).to_owned())
            .collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_and_codex_interrupt_with_escape() {
        assert_eq!(interrupt_bytes(AgentKind::Claude), Some(b"\x1b".to_vec()));
        assert_eq!(interrupt_bytes(AgentKind::Codex), Some(b"\x1b".to_vec()));
    }

    #[test]
    fn grok_needs_two_control_c_and_never_escape() {
        // [V] grok-signals-1 A4: `Esc` keeps the turn and the draft; the first
        // Ctrl+C only clears the draft. Sending one of either and reporting
        // "interrupted" would be a lie the UI then shows the user.
        assert_eq!(
            interrupt_bytes(AgentKind::Grok),
            Some(b"\x03\x03".to_vec()),
            "one Ctrl+C only clears the draft"
        );
    }

    #[test]
    fn an_unmeasured_harness_refuses_rather_than_guessing() {
        assert_eq!(
            interrupt_bytes(AgentKind::Agy),
            None,
            "§6: agy is unknown, and unknown must not be spelled as a keypress"
        );
        assert_eq!(interrupt_bytes(AgentKind::Terminal), None);
        assert_eq!(interrupt_bytes(AgentKind::Generic), None);
    }

    #[test]
    fn only_codex_reports_a_native_queue() {
        // §6 mapping rule 2: the emulated queue is the default and codex's
        // `Tab` is the single exception. If this ever flips silently the UI
        // starts claiming Remuda's own ledger is the harness's.
        for row in HARNESS_KEYS {
            match row.kind {
                AgentKind::Codex => {
                    assert_eq!(row.queue, Provision::Native);
                    assert_eq!(row.queue_key, Some("tab"));
                }
                AgentKind::Agy => assert_eq!(row.queue, Provision::Unknown),
                _ => {
                    assert_eq!(row.queue, Provision::Emulated);
                    assert_eq!(row.queue_key, None, "an emulated queue has no native key");
                }
            }
        }
    }

    #[test]
    fn only_grok_charges_a_turn_for_sending_now() {
        for row in HARNESS_KEYS {
            assert_eq!(
                row.send_now_costs_turn,
                row.kind == AgentKind::Grok,
                "{:?} disagrees with §6's send-now column",
                row.kind
            );
        }
    }

    #[test]
    fn every_harness_with_an_emulated_queue_has_a_turn_end_signal_to_deliver_on() {
        // An emulated queue with nothing to wait for would hold the message
        // forever; §6 rule 2 names the signal for each.
        for row in HARNESS_KEYS {
            if row.queue == Provision::Emulated {
                assert!(
                    turn_end_signal(row.kind).is_some(),
                    "{:?} queues in Remuda but has no turn-end signal",
                    row.kind
                );
            }
        }
    }

    #[test]
    fn provisions_map_onto_the_wire_values_the_ui_branches_on() {
        assert_eq!(Provision::Native.wire(), CapabilityProvision::Native);
        assert_eq!(Provision::Emulated.wire(), CapabilityProvision::Emulated);
        assert_eq!(Provision::Unknown.wire(), CapabilityProvision::Unknown);
    }
}
