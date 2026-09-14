//! The prompt delivery ready ladder (D-028 §5.2).
//!
//! `agent.prompt` used to hide two decisions inside herdr: *when* the composer
//! is ready for text, and *how* that text has to be spelled. Both have to be
//! rebuilt here, and both have a failure mode that looks like success:
//!
//! * Writing bytes to a PTY always "succeeds". The TUI may still have been
//!   mid-repaint and dropped them. Hence the ladder — take the strongest
//!   available evidence that the composer is listening, never assume.
//! * `ESC[200~` is bracketed paste only to an application that asked for it.
//!   To one that did not, it is six literal characters typed into the prompt.
//!   Hence gating it on the emulator having actually observed `?2004`, rather
//!   than on "this is an agent, agents support paste".

use remuda_screen::ModeSet;
use std::time::Duration;

/// Minimum screen silence before Enter is sent. §5.2.
///
/// Replaces the flat 500 ms sleep: a TUI that has already settled should not
/// cost half a second per prompt, and one that has not settled should get more
/// than a fixed guess.
pub(super) const QUIESCENCE: Duration = Duration::from_millis(80);

/// Upper bound on waiting for silence. §5.2.
///
/// A TUI rendering a spinner is never quiet. Capping means a prompt is
/// delivered late rather than never, and the cap equals the old fixed delay, so
/// the worst case is exactly the pre-D-028 behaviour.
pub(super) const QUIESCENCE_CAP: Duration = Duration::from_millis(500);

/// How often quiescence is sampled.
pub(super) const QUIESCENCE_POLL: Duration = Duration::from_millis(10);

/// Which rung of the ladder decided the composer was ready.
///
/// Journaled with the delivery so an operator debugging a lost prompt can see
/// whether Remuda *knew* the composer took it or merely believed the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyEvidence {
    /// Rung 1: the harness's own `UserPromptSubmit` hook fired for the previous
    /// prompt, so the hook path is live and its receipt is trustworthy.
    HookReceipt,
    /// Rung 2: the emulator observed the composer's mode set and the screen
    /// then went quiet.
    Quiescence,
    /// Rung 3: a screen signature said idle. Weakest, and the only rung that
    /// can be wrong about a dialog that merely looks like a prompt box.
    Glyph,
    /// Not a promoted agent — a plain shell always takes bytes.
    Shell,
}

impl ReadyEvidence {
    /// Wire name for the journal.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::HookReceipt => "hook-receipt",
            Self::Quiescence => "emulator-quiescence",
            Self::Glyph => "glyph",
            Self::Shell => "shell",
        }
    }
}

/// How one prompt is spelled for one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The composer body, written first.
    pub body: Vec<u8>,
    /// The submit key, written separately once the screen is quiet.
    ///
    /// `None` for a plain shell, where the newline belongs to the same write
    /// (a shell reads a line, not a composer).
    pub submit: Option<Vec<u8>>,
    /// Whether the body is bracketed-paste wrapped.
    pub bracketed: bool,
}

/// Encode `text` for delivery.
///
/// `promoted` distinguishes the two shapes; `modes` is the emulator's
/// observation of the target's DECSET state, and is what decides bracketed
/// paste. Passing `None` — the emulator is off — means *never* bracket, which
/// is the safe direction: an un-bracketed multi-line paste submits once per
/// line, which is visible and recoverable, while a literal `ESC[200~` in the
/// prompt corrupts the message silently.
#[must_use]
pub fn encode(text: &str, promoted: bool, modes: Option<ModeSet>) -> Delivery {
    if !promoted {
        let mut body = text.as_bytes().to_vec();
        if !body.ends_with(b"\r") && !body.ends_with(b"\n") {
            body.push(b'\r');
        }
        return Delivery {
            body,
            submit: None,
            bracketed: false,
        };
    }
    let trimmed = text.trim_end_matches(['\r', '\n']);
    let multiline = trimmed.contains('\n') || trimmed.contains('\r');
    // §5.2: paste only when the application asked for it. A multi-line prompt
    // to a TUI that never set `?2004` is sent raw and will submit per line —
    // wrong, but honestly wrong, and the alternative writes escape bytes into
    // the user's message.
    let bracketed = multiline && modes.is_some_and(|modes| modes.bracketed_paste);
    let mut body = Vec::with_capacity(trimmed.len() + 16);
    if bracketed {
        body.extend_from_slice(b"\x1b[200~");
        body.extend_from_slice(trimmed.as_bytes());
        body.extend_from_slice(b"\x1b[201~");
    } else {
        body.extend_from_slice(trimmed.as_bytes());
    }
    Delivery {
        body,
        // [V] claude-queue-steer-1: a CR inside the paste does not submit, and
        // body+CR in one write does not submit either. The Enter has to be its
        // own write, for every promoted target.
        submit: Some(vec![b'\r']),
        bracketed,
    }
}

/// Pick the strongest available rung.
///
/// `hook_receipt` — the hook socket has produced a `UserPromptSubmit` for this
/// session, so rung 1 is real rather than configured.
/// `modes` — `Some` when the emulator is running.
/// `glyph_idle` — the screen signature says idle.
#[must_use]
pub fn ready_rung(
    promoted: bool,
    hook_receipt: bool,
    modes: Option<ModeSet>,
    glyph_idle: bool,
) -> Option<ReadyEvidence> {
    if !promoted {
        return Some(ReadyEvidence::Shell);
    }
    if hook_receipt {
        return Some(ReadyEvidence::HookReceipt);
    }
    if modes.is_some() {
        return Some(ReadyEvidence::Quiescence);
    }
    if glyph_idle {
        return Some(ReadyEvidence::Glyph);
    }
    // §5.2: an `unknown` screen queues the prompt; it does not hard-send it.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(bracketed: bool) -> ModeSet {
        ModeSet {
            bracketed_paste: bracketed,
            ..ModeSet::default()
        }
    }

    #[test]
    fn a_shell_prompt_is_one_write_ending_in_a_carriage_return() {
        let delivery = encode("ls -la", false, None);
        assert_eq!(delivery.body, b"ls -la\r");
        assert_eq!(
            delivery.submit, None,
            "a shell reads a line, not a composer"
        );
        assert!(!delivery.bracketed);
    }

    #[test]
    fn a_shell_prompt_that_already_ends_in_a_newline_is_not_doubled() {
        assert_eq!(encode("ls\n", false, None).body, b"ls\n");
    }

    #[test]
    fn a_promoted_prompt_always_sends_its_enter_separately() {
        // [V] claude-queue-steer-1: body+CR in a single write is accepted as
        // bytes and never submitted.
        let delivery = encode("hello", true, Some(modes(true)));
        assert_eq!(delivery.body, b"hello");
        assert_eq!(delivery.submit, Some(b"\r".to_vec()));
    }

    #[test]
    fn bracketed_paste_requires_the_emulator_to_have_seen_2004() {
        let observed = encode("first\nsecond", true, Some(modes(true)));
        assert!(observed.bracketed);
        assert_eq!(observed.body, b"\x1b[200~first\nsecond\x1b[201~".to_vec());

        // The same text to a target that never asked for paste: sending the
        // brackets would type `ESC[200~` into the composer as literal text.
        let unobserved = encode("first\nsecond", true, Some(modes(false)));
        assert!(!unobserved.bracketed);
        assert_eq!(unobserved.body, b"first\nsecond".to_vec());
    }

    #[test]
    fn without_an_emulator_nothing_is_bracketed() {
        // The ring cannot observe DECSET, so "did it ask for paste" is
        // unanswerable. §5.2 makes that a no.
        let delivery = encode("first\nsecond", true, None);
        assert!(!delivery.bracketed);
        assert_eq!(delivery.body, b"first\nsecond".to_vec());
    }

    #[test]
    fn a_single_line_prompt_is_never_bracketed_even_when_paste_is_available() {
        // Bracketing exists to stop a newline submitting early. One line has no
        // newline to protect, and the wrapper is pure risk.
        let delivery = encode("hello", true, Some(modes(true)));
        assert!(!delivery.bracketed);
        assert_eq!(delivery.body, b"hello");
    }

    #[test]
    fn a_trailing_newline_becomes_the_submit_rather_than_part_of_the_body() {
        let delivery = encode("hello\n", true, Some(modes(true)));
        assert_eq!(delivery.body, b"hello");
        assert_eq!(delivery.submit, Some(b"\r".to_vec()));
    }

    #[test]
    fn the_ladder_prefers_a_hook_receipt_to_the_screen() {
        assert_eq!(
            ready_rung(true, true, Some(modes(true)), true),
            Some(ReadyEvidence::HookReceipt)
        );
    }

    #[test]
    fn the_ladder_falls_to_quiescence_then_glyph() {
        assert_eq!(
            ready_rung(true, false, Some(modes(true)), false),
            Some(ReadyEvidence::Quiescence)
        );
        assert_eq!(
            ready_rung(true, false, None, true),
            Some(ReadyEvidence::Glyph)
        );
    }

    #[test]
    fn an_unknown_screen_with_no_stronger_evidence_yields_no_rung() {
        // §5.2: "unknown 时排队而不是硬发". Returning a rung here would type a
        // prompt into whatever dialog happens to be up.
        assert_eq!(ready_rung(true, false, None, false), None);
    }

    #[test]
    fn a_plain_shell_is_always_ready() {
        assert_eq!(
            ready_rung(false, false, None, false),
            Some(ReadyEvidence::Shell)
        );
    }

    #[test]
    fn rung_labels_are_stable_for_the_journal() {
        assert_eq!(ReadyEvidence::HookReceipt.label(), "hook-receipt");
        assert_eq!(ReadyEvidence::Quiescence.label(), "emulator-quiescence");
        assert_eq!(ReadyEvidence::Glyph.label(), "glyph");
        assert_eq!(ReadyEvidence::Shell.label(), "shell");
    }
}
