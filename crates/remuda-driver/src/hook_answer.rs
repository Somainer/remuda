//! The one-shot budget for screen-key fallbacks (D-028 §4.4, §14 risk 1).
//!
//! When a hook decision does not reach the agent, the driver answers the
//! dialog on screen instead. [`shell_pty::answer`](crate::shell_pty) decides
//! *which keys*; this module decides *whether we are still allowed to press
//! any*, and [`Applied`] names what actually happened afterwards.
//!
//! # Why the fallback is armed by proof, not by sight
//!
//! §14 risk 1 records that an allow can be ignored, and blames confined
//! sessions. Measuring claude 2.1.221 for
//! `docs/design/evidence/native-pty-5.md` turned up a second cause and, more
//! importantly, a trap:
//!
//! - A reply in the wrong shape (a bare `{"behavior":"allow"}` rather than the
//!   nested `hookSpecificOutput` form) is dropped **silently** — the dialog
//!   stays up and the tool is refused, with nothing in the reply to say so.
//! - The agent renders its dialog *while the hook is still pending*. A 75 s
//!   hook left the prompt on screen the entire time, and the late allow still
//!   applied and wrote the file.
//!
//! So "a dialog is visible" is not evidence of anything: it is the normal
//! state of a healthy pending approval, and pressing a key then would answer
//! twice — once by hook, once by keyboard. The fallback is armed only when the
//! hook path has *proven* it did not take the decision
//! ([`needs_fallback`]).
//!
//! And it runs once. If a keypress landed but its ACK was lost, replaying it
//! answers whatever dialog is showing *now*, which may be a different
//! question — D-022's "never replay Enter", applied to this path.

use remuda_protocol::InteractionId;
use remuda_signal::Outcome;
use std::collections::BTreeSet;
use std::sync::Mutex;

/// Whether a decision reached the agent, and how sure we are.
///
/// Returned rather than a `bool` so a caller cannot read "written" as
/// "applied" — that distinction is the whole of §14 risk 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The parked hook received the decision. Confirmed.
    ByHook,
    /// The hook did not take it; a screen key was pressed and the dialog was
    /// then observed to clear. Confirmed by screen change.
    ByScreenKey,
    /// Keys went out but the dialog did not clear, or nothing could be
    /// pressed. Must be reported to the user as *not applied*.
    NotApplied,
}

impl Applied {
    /// Journal spelling.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ByHook => "hook",
            Self::ByScreenKey => "screen-key-fallback",
            Self::NotApplied => "not-applied",
        }
    }

    /// True when the agent is known to have taken the decision.
    ///
    /// Both confirmed paths qualify, and they are the only two: a keypress
    /// counts only *after* the dialog was seen to clear, never on the strength
    /// of having been written.
    #[must_use]
    pub fn is_confirmed(self) -> bool {
        matches!(self, Self::ByHook | Self::ByScreenKey)
    }
}

/// Whether the hook path's outcome leaves anything for the screen to do.
///
/// [`Outcome::Answered`] is the end of the story. The other two both mean
/// nothing received the decision, so the screen is worth one attempt.
#[must_use]
pub fn needs_fallback(outcome: Outcome) -> bool {
    match outcome {
        Outcome::Answered => false,
        Outcome::Abandoned | Outcome::TimedOut => true,
    }
}

/// Tracks which interactions have spent their one fallback attempt.
///
/// Per driver, not per call: the budget has to outlive the call that spends
/// it, or "exactly once" degrades to "once per attempt".
#[derive(Default)]
pub struct FallbackLedger {
    spent: Mutex<BTreeSet<String>>,
}

impl FallbackLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the single fallback attempt for `id`. True exactly once.
    ///
    /// Callers claim *before* writing keys, never after: when a write's
    /// outcome is unknown, D-022 requires assuming it landed.
    #[must_use]
    pub fn claim(&self, id: &InteractionId) -> bool {
        self.spent
            .lock()
            .map(|mut spent| spent.insert(id.as_id().to_string()))
            // A poisoned ledger cannot prove the attempt is unspent, and the
            // safe reading of "don't know" is "already used".
            .unwrap_or(false)
    }

    /// Whether `id` has already used its attempt.
    #[must_use]
    pub fn is_spent(&self, id: &InteractionId) -> bool {
        self.spent
            .lock()
            .map(|spent| spent.contains(id.as_id().as_str()))
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_confirmed_hook_delivery_ends_the_story() {
        assert!(!needs_fallback(Outcome::Answered));
        // Both failures mean nothing received the decision.
        assert!(needs_fallback(Outcome::Abandoned));
        assert!(needs_fallback(Outcome::TimedOut));
    }

    #[test]
    fn a_keypress_alone_is_never_confirmation() {
        // ByScreenKey is only constructed after the dialog was seen to clear;
        // the unconfirmed case is NotApplied, and it must not read as success.
        assert!(Applied::ByHook.is_confirmed());
        assert!(Applied::ByScreenKey.is_confirmed());
        assert!(!Applied::NotApplied.is_confirmed());
        assert_eq!(Applied::ByHook.label(), "hook");
        assert_eq!(Applied::ByScreenKey.label(), "screen-key-fallback");
        assert_eq!(Applied::NotApplied.label(), "not-applied");
    }

    #[test]
    fn the_fallback_is_available_exactly_once_per_interaction() {
        // A replayed Enter answers whatever dialog is on screen *now*, which
        // may be a different question entirely (D-022).
        let ledger = FallbackLedger::new();
        let id = InteractionId::new();
        assert!(ledger.claim(&id), "the first attempt is allowed");
        assert!(!ledger.claim(&id), "a second attempt must be refused");
        assert!(ledger.is_spent(&id));
    }

    #[test]
    fn each_interaction_has_its_own_budget() {
        let ledger = FallbackLedger::new();
        let first = InteractionId::new();
        let second = InteractionId::new();
        assert!(ledger.claim(&first));
        assert!(!ledger.is_spent(&second));
        assert!(
            ledger.claim(&second),
            "one spent budget must not block another"
        );
    }
}
