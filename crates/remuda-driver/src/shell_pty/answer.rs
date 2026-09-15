//! Screen-key fallback when a hook decision does not reach the agent
//! (D-028 §4.4 tier A → tier D, §14 risk 1).
//!
//! A `PermissionRequest` normally blocks on the socket and the human's answer
//! returns through it. But the measured reality (claude 2.1.221,
//! `docs/design/evidence/native-pty-5.md`) is that the reply can be ignored —
//! a confined session accepts only command-line authorisation, and a reply in
//! the wrong shape is dropped silently — while the harness keeps its own
//! on-screen dialog up. The human *did* answer and the agent did not hear, so
//! we answer the dialog on screen exactly once and then confirm it cleared.
//!
//! This module is the pure half: given the device's answer and the rendered
//! grid, say which keys select it, and say whether the dialog has gone. The
//! write half — claim the single attempt before writing, never replay — stays
//! in the driver, where the D-022 invariants are enforced.

use remuda_protocol::InteractionAnswer;
use remuda_screen::{ScreenChoice, ScreenGrid, screen_request};

/// What the human meant, independent of how it is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Approve this one call.
    AllowOnce,
    /// Approve and persist the grant the harness offered.
    AllowAlways,
    /// Refuse.
    Deny,
}

/// Read the intent out of a hook-card answer.
///
/// The option ids are the ones the card was built with in
/// `remuda_signal::approval`. Returns `None` for an answer shape a permission
/// fallback cannot turn into a keypress (a text question, a plan review).
#[must_use]
pub fn intent_of(answer: &InteractionAnswer) -> Option<Intent> {
    let InteractionAnswer::Approval(answer) = answer else {
        return None;
    };
    let id = answer.option_id.as_str();
    if id == remuda_signal::approval::DENY {
        Some(Intent::Deny)
    } else if id == remuda_signal::approval::ALLOW_ONCE {
        Some(Intent::AllowOnce)
    } else if remuda_signal::approval::allow_always_index(id).is_some() {
        Some(Intent::AllowAlways)
    } else {
        None
    }
}

/// Why a screen fallback could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackError {
    /// No unambiguous approval dialog is on screen, so sending keys would be a
    /// guess — exactly what D-022 forbids.
    NoDialog,
    /// The screen was truncated; answering would be answering blind.
    Truncated,
    /// The dialog offers no choice that matches the human's intent.
    NoMatchingChoice,
}

impl std::fmt::Display for FallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDialog => write!(f, "no unambiguous approval dialog on screen"),
            Self::Truncated => write!(f, "the approval screen is truncated"),
            Self::NoMatchingChoice => write!(f, "no on-screen choice matches that decision"),
        }
    }
}

/// The logical keys that select the answer on the dialog currently rendered.
///
/// D-022 gate, applied here because this is a screen answer: the whole screen
/// must be present, the parse must be unambiguous, and it has to read as an
/// approval rather than an unrelated menu (a transcript viewer and an approval
/// can look alike — design §10 anchor ③).
pub fn keys_for(
    grid: &ScreenGrid,
    answer: &InteractionAnswer,
) -> Result<Vec<String>, FallbackError> {
    let intent = intent_of(answer).ok_or(FallbackError::NoDialog)?;
    let parsed = screen_request(grid);
    if parsed.truncated {
        return Err(FallbackError::Truncated);
    }
    if parsed.ambiguous || parsed.choices.is_empty() || !parsed.approval {
        return Err(FallbackError::NoDialog);
    }
    let choice = pick_choice(&parsed.choices, intent).ok_or(FallbackError::NoMatchingChoice)?;
    Ok(choice.keys.clone())
}

/// Choose the on-screen entry that means the same thing as `intent`.
///
/// Matching is by label rather than by position: the option set varies by tool
/// (a Bash approval and a Write approval do not offer the same choices), so a
/// fixed index would eventually press the wrong one.
fn pick_choice(choices: &[ScreenChoice], intent: Intent) -> Option<&ScreenChoice> {
    let lower: Vec<String> = choices
        .iter()
        .map(|choice| choice.label.to_lowercase())
        .collect();
    let is_deny = |label: &str| {
        label.starts_with("no")
            || label.starts_with("n (")
            || label.contains("deny")
            || label.contains("reject")
            || label.contains("refuse")
    };
    // "Yes, allow all edits during this session" also starts with "yes";
    // treating it as the plain yes would grant a session-wide permission the
    // human never chose, so persistence is tested first and wins.
    let is_persistent = |label: &str| {
        label.contains("always")
            || label.contains("allow all")
            || label.contains("all edits")
            || label.contains("this session")
            || label.contains("don't ask")
            || label.contains("do not ask")
    };
    let deny_index = lower.iter().position(|label| is_deny(label));
    let persistent_index = lower
        .iter()
        .position(|label| !is_deny(label) && is_persistent(label));
    let once_index = (0..choices.len())
        .find(|index| Some(*index) != deny_index && Some(*index) != persistent_index);
    match intent {
        Intent::Deny => deny_index.map(|index| &choices[index]),
        // An always-allow the screen cannot express degrades to a plain yes:
        // the human said allow, and allowing once is the part of that this
        // dialog can do. Doing nothing would be worse.
        Intent::AllowAlways => persistent_index.or(once_index).map(|index| &choices[index]),
        Intent::AllowOnce => once_index.map(|index| &choices[index]),
    }
}

/// Whether the approval dialog has left the screen.
///
/// "Never report a decision as applied unless confirmed (screen change)" is
/// the half of the honesty rule a write cannot satisfy by itself. The caller
/// polls this for a short window; `true` only once the grid no longer parses
/// as an approval.
#[must_use]
pub fn dialog_cleared(grid: &ScreenGrid) -> bool {
    let parsed = screen_request(grid);
    !parsed.approval || parsed.choices.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_protocol::{ApprovalAnswer, Digest};

    fn grid(text: &str) -> ScreenGrid {
        ScreenGrid::from_raw(text)
    }

    fn answer(option: &str) -> InteractionAnswer {
        InteractionAnswer::Approval(Box::new(ApprovalAnswer {
            option_id: option.into(),
            input_digest: Digest::try_from(format!("sha256:{:0>64}", "a")).unwrap(),
        }))
    }

    /// The measured claude 2.1.221 Write approval dialog.
    const CLAUDE_DIALOG: &str = "Do you want to create probe.txt?\n\
        \u{276f} 1. Yes\n\
        2. Yes, allow all edits during this session (shift+tab)\n\
        3. No";

    #[test]
    fn allow_once_selects_the_plain_yes() {
        // Cursor-relative: the `❯` already sits on "Yes", so Enter alone takes
        // it. A menu with a cursor is not listening for digits.
        assert_eq!(
            keys_for(&grid(CLAUDE_DIALOG), &answer("allow-once")).unwrap(),
            vec!["enter".to_owned()]
        );
    }

    #[test]
    fn allow_always_selects_the_session_grant_not_the_plain_yes() {
        // "Yes, allow all edits during this session" starts with "Yes"; if it
        // were matched as the plain yes the grant would silently not persist.
        assert_eq!(
            keys_for(&grid(CLAUDE_DIALOG), &answer("allow-always-0")).unwrap(),
            vec!["down".to_owned(), "enter".to_owned()]
        );
    }

    #[test]
    fn an_allow_once_never_presses_the_session_wide_option() {
        // The converse: allow-once must skip the persistent entry even when it
        // comes first on screen.
        let reordered = "Do you want to create probe.txt?\n\
            1. Yes, allow all edits during this session\n\
            2. Yes\n\
            3. No";
        // No cursor on this screen, so the choices keep their digit keys.
        assert_eq!(
            keys_for(&grid(reordered), &answer("allow-once")).unwrap(),
            vec!["2".to_owned(), "enter".to_owned()]
        );
    }

    #[test]
    fn deny_selects_no() {
        assert_eq!(
            keys_for(&grid(CLAUDE_DIALOG), &answer("deny")).unwrap(),
            vec!["down".to_owned(), "down".to_owned(), "enter".to_owned()]
        );
    }

    #[test]
    fn an_always_allow_the_screen_cannot_express_degrades_to_yes() {
        let screen = "Run this command? [y/n]";
        assert_eq!(
            keys_for(&grid(screen), &answer("allow-always-0")).unwrap(),
            vec!["y".to_owned(), "enter".to_owned()]
        );
    }

    #[test]
    fn a_yes_no_dialog_maps_allow_to_y_and_deny_to_n() {
        let screen = "Run this command? [y/n]";
        assert_eq!(
            keys_for(&grid(screen), &answer("allow-once")).unwrap(),
            vec!["y".to_owned(), "enter".to_owned()]
        );
        assert_eq!(
            keys_for(&grid(screen), &answer("deny")).unwrap(),
            vec!["n".to_owned(), "enter".to_owned()]
        );
    }

    #[test]
    fn a_dialog_that_is_not_an_approval_cannot_be_answered_as_one() {
        // A model picker and an approval look alike (§10 anchor ③); pressing
        // a key into the wrong one silently changes the session.
        let screen = "Select environment:\n\u{276f} 1. Development\n2. Staging";
        assert_eq!(
            keys_for(&grid(screen), &answer("allow-once")),
            Err(FallbackError::NoDialog)
        );
    }

    #[test]
    fn a_truncated_screen_is_refused() {
        // The D-022 blind-answer prohibition: a hidden option may be the one
        // the cursor is actually on.
        let long = "x".repeat(6000) + "\n" + CLAUDE_DIALOG;
        assert_eq!(
            keys_for(&grid(&long), &answer("allow-once")),
            Err(FallbackError::Truncated)
        );
    }

    #[test]
    fn a_question_answer_has_no_screen_intent() {
        let answer = InteractionAnswer::Question(Box::new(remuda_protocol::QuestionAnswer {
            answers: std::collections::BTreeMap::new(),
        }));
        assert_eq!(
            keys_for(&grid(CLAUDE_DIALOG), &answer),
            Err(FallbackError::NoDialog)
        );
    }

    #[test]
    fn clearing_is_detected_only_when_the_approval_is_gone() {
        // This is the evidence that turns "keys written" into "applied".
        assert!(!dialog_cleared(&grid(CLAUDE_DIALOG)));
        assert!(dialog_cleared(&grid("Wrote 1 line to probe.txt")));
    }

    #[test]
    fn intent_maps_every_card_option_and_nothing_else() {
        assert_eq!(intent_of(&answer("allow-once")), Some(Intent::AllowOnce));
        assert_eq!(
            intent_of(&answer("allow-always-2")),
            Some(Intent::AllowAlways)
        );
        assert_eq!(intent_of(&answer("deny")), Some(Intent::Deny));
        assert_eq!(intent_of(&answer("bogus")), None);
    }
}
