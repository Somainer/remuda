//! Screen signatures: what the terminal looks like when an agent is idle,
//! working, blocked, or booting (D-028 §4.1, §10).
//!
//! Extracted verbatim from `remuda-driver`'s `promote.rs`. The matching rules
//! are unchanged — only the input type moved, from an ANSI-stripped byte tail
//! to a [`ScreenGrid`]. §10's rule table will replace the hard-coded phrases
//! here in P7; until then the phrases live in one place with golden coverage.

use crate::grid::ScreenGrid;
use remuda_protocol::AgentKind;

/// Screen-derived status of a promoted agent TUI.
///
/// Same class of evidence `claude-pty` takes from herdr's `agent_status`, read
/// here off the terminal instead. It is a heuristic over rendered text, so it
/// is reported as screen-derived and never treated as proof of task success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenStatus {
    /// An input box is on screen and accepting a prompt.
    Idle,
    /// A turn is running; the TUI offers `esc to interrupt`.
    Working,
    /// A native dialog owns the keyboard; a prompt would answer it by accident.
    Blocked,
}

impl ScreenStatus {
    /// Wire label matching the `agent_status` values Node already folds into
    /// [`remuda_protocol::Activity`].
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// Phrases that mean a native dialog owns the keyboard.
const BLOCKED_PHRASES: &[&str] = &[
    "Do you want to",
    "Is this a project you created or one you trust?",
    "Yes, I trust this folder",
];

/// Phrase the TUI shows only while a turn is in flight.
const WORKING_PHRASE: &str = "esc to interrupt";

/// The composer's prompt glyph (`❯`).
const PROMPT_GLYPH: char = '\u{276f}';

/// Classify a promoted agent TUI's screen.
///
/// Blocked is checked first: mistaking a dialog for an idle prompt is the one
/// error that silently answers a question the human never saw (D-022).
///
/// The composer box is the TUI's "ready for a prompt" signal. A full-screen TUI
/// repaints with cursor motion rather than newlines, so the prompt glyph is not
/// reliably at the start of a line — look for the glyph itself, not for a line
/// that begins with it.
#[must_use]
pub fn screen_status(grid: &ScreenGrid) -> Option<ScreenStatus> {
    let flat = grid.flat();
    if BLOCKED_PHRASES.iter().any(|phrase| flat.contains(phrase)) {
        return Some(ScreenStatus::Blocked);
    }
    if flat.contains(WORKING_PHRASE) {
        return Some(ScreenStatus::Working);
    }
    grid.text()
        .contains(PROMPT_GLYPH)
        .then_some(ScreenStatus::Idle)
}

/// Last-resort agent detection over the screen, for when the PTY's foreground
/// process group is unavailable (D-025).
///
/// Deliberately narrow: only the Claude TUI's own banner. Widening this is a
/// P7 rule-table job, not a here-and-now heuristic — a false promotion changes
/// how every later prompt is delivered.
#[must_use]
pub fn detect_from_screen(grid: &ScreenGrid) -> Option<AgentKind> {
    let lower = grid.text().to_ascii_lowercase();
    (lower.contains("welcome to claude code") || lower.contains("claude code v"))
        .then_some(AgentKind::Claude)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(screen: &str) -> ScreenGrid {
        ScreenGrid::from_raw(screen)
    }

    #[test]
    fn an_idle_composer_accepts_a_prompt() {
        let screen = "\n\u{2500}\u{2500}\u{2500}\n\u{276f} Try \"how do I log an error?\"\n\u{2500}\u{2500}\u{2500}\n";
        assert_eq!(screen_status(&raw(screen)), Some(ScreenStatus::Idle));
        assert_eq!(ScreenStatus::Idle.label(), "idle");
    }

    #[test]
    fn a_running_turn_reads_as_working_not_idle() {
        let screen = "\u{276f} hello\n\u{2726} Thinking… (esc to interrupt)\n";
        assert_eq!(
            screen_status(&raw(screen)),
            Some(ScreenStatus::Working),
            "a turn in flight must not look ready for the next prompt"
        );
    }

    #[test]
    fn a_native_dialog_reads_as_blocked_even_with_a_composer_on_screen() {
        // A prompt typed here would silently answer the dialog (D-022).
        let screen = "\u{276f} earlier\nQuick safety check:\nIs this a project you created or one you trust?\n\u{276f} Yes, I trust this folder\n";
        assert_eq!(screen_status(&raw(screen)), Some(ScreenStatus::Blocked));
    }

    #[test]
    fn a_booting_or_plain_shell_screen_has_no_status_yet() {
        assert_eq!(screen_status(&raw("$ claude\n")), None);
        assert_eq!(screen_status(&raw("")), None);
    }

    #[test]
    fn a_soft_wrapped_dialog_still_reads_as_blocked() {
        // §10 anchor ⑤: a narrow terminal wraps the question mid-phrase, and a
        // `contains` over the unwrapped text would miss it.
        let grid = ScreenGrid::from_lines(["Is this a project you created", "or one you trust?"]);
        assert_eq!(screen_status(&grid), Some(ScreenStatus::Blocked));
    }

    #[test]
    fn screen_fallback_only_fires_on_the_claude_banner() {
        assert_eq!(
            detect_from_screen(&raw("\n ✻ Welcome to Claude Code!\n")),
            Some(AgentKind::Claude)
        );
        assert_eq!(detect_from_screen(&raw("$ ls -la\ntotal 12\n")), None);
    }
}
