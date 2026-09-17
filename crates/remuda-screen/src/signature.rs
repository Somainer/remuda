//! Screen signatures: what the terminal looks like when an agent is idle,
//! working, blocked, or booting (D-028 §4.1, §10).
//!
//! Extracted verbatim from `remuda-driver`'s `promote.rs`. The matching rules
//! now read the OSC tier first (design §2.4) — claude 2.1.270 removed the
//! `esc to interrupt` footer phrase this matcher used to key on, so without
//! OSC the screen tier reported `Idle` **during a running turn** (D-2).
//!
//! Two policies sit on top of the pure read:
//!
//! - **Raise-only** (design §0.2 rule 6): OSC may raise `busy`; it may not
//!   clear it while a higher tier is alive. [`ScreenLatch`] holds busy across
//!   an `OSC 9;4;0` until a hook-or-file authority idles the turn, because
//!   that sequence can race the turn end and the hook always wins that race.
//! - **Blocked latch** (D-028 §10 anchor ④): a flickering `⚠ Action
//!   Required` frame drops on blur, so once blocked is seen it latches until a
//!   positive idle/working signal.

use crate::grid::ScreenGrid;
use crate::osc;
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

/// Health of the hook tier at one poll, the input to the raise-only rule.
///
/// Mirrors the `ChannelHealth.reason` vocabulary the browser derives from the
/// journal (design §2.6). The latch only needs to know whether a higher
/// authority than the screen is alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookHealth {
    /// A hook record arrived this run. The screen tier must not end a turn —
    /// `Stop` owns that edge.
    Healthy,
    /// The hook tier was expected but produced nothing at all (D-4's
    /// shape). The screen tier is the best authority and *may* lower busy.
    NeverMaterialised,
    /// Records arrived earlier but the tier has gone quiet past its cadence.
    /// Treated like [`Self::NeverMaterialised`] for the lowering decision.
    Stalled,
}

impl HookHealth {
    /// Whether the hook tier is live authority the screen must defer to.
    #[must_use]
    fn alive(self) -> bool {
        self == Self::Healthy
    }
}

/// Phrases that mean a native dialog owns the keyboard.
const BLOCKED_PHRASES: &[&str] = &[
    "Do you want to",
    "Is this a project you created or one you trust?",
    "Yes, I trust this folder",
    // Auto-mode outside-reads dialog (dispatch-onboarding-1): a first-run
    // modal the Hub watch classifier must not mistake for a working turn.
    "Allow reads outside the working directories?",
];

/// Phrase the TUI shows only while a turn is in flight.
///
/// Legacy builds only: absent from a 22,521-byte claude 2.1.270 capture, so
/// this is a fallback behind the OSC tier, never the primary working signal.
const WORKING_PHRASE: &str = "esc to interrupt";

/// The composer's prompt glyph (`❯`).
const PROMPT_GLYPH: char = '\u{276f}';

/// Classify a promoted agent TUI's screen.
///
/// Precedence, per design §2.4:
///
/// 1. `OSC 9;4;3` + `✳` title → [`ScreenStatus::Blocked`] (the `✳` glyph is
///    idle *or* needs-input; progress still busy disambiguates).
/// 2. `OSC 9;4;3` (or a `◐`/`◑` title) → [`ScreenStatus::Working`].
/// 3. [`BLOCKED_PHRASES`] → [`ScreenStatus::Blocked`] (dialogs first of the
///    text rules; mistaking one for idle silently answers a question nobody
///    saw, D-022).
/// 4. [`WORKING_PHRASE`] → [`ScreenStatus::Working`] (pre-2.1.270 builds).
/// 5. Prompt glyph with no active progress → [`ScreenStatus::Idle`].
/// 6. Anything else → `None`: unknown never collapses to idle (D-028 §10).
///
/// Blocked is checked first among the *text* rules: the OSC busy edges above
/// still win, because a dialog keeps progress at indeterminate.
#[must_use]
pub fn screen_status(grid: &ScreenGrid) -> Option<ScreenStatus> {
    let status = osc::osc_status(&grid.osc);
    if status.needs_input {
        return Some(ScreenStatus::Blocked);
    }
    if status.busy {
        return Some(ScreenStatus::Working);
    }
    let flat = grid.flat();
    if BLOCKED_PHRASES.iter().any(|phrase| flat.contains(phrase)) {
        return Some(ScreenStatus::Blocked);
    }
    if flat.contains(WORKING_PHRASE) {
        return Some(ScreenStatus::Working);
    }
    // The composer box is the TUI's "ready for a prompt" signal, but an
    // active progress bar vetoes it: a `1;-1`/`2;…` payload is *known work*,
    // even though it is not claude's indeterminate `3`.
    if osc::progress_active(&status) {
        return None;
    }
    grid.text()
        .contains(PROMPT_GLYPH)
        .then_some(ScreenStatus::Idle)
}

/// Stateful screen classification across promotion polls.
///
/// [`screen_status`] is a pure read of one grid; the raise-only and
/// blocked-latch policies both need the previous verdict, which lives here.
/// One instance per promoted PTY, fed on the same poll cadence as the
/// emulator grid.
#[derive(Debug, Default)]
pub struct ScreenLatch {
    /// A low tier (OSC/screen) raised busy and has not been lowered by an
    /// authority allowed to.
    raised: bool,
    /// A blocked frame was seen and no positive idle/working signal cleared
    /// it yet.
    blocked: bool,
    /// The latched blocked came from the OSC title edge (`✳` + busy progress)
    /// rather than a dialog phrase. That edge flickers dropped frames (anchor
    /// ④), so its positive idle signal must be the explicit `9;4;0` edge —
    /// the omnipresent composer glyph is not enough.
    osc_blocked: bool,
    /// Last verdict, exposed for callers that want the held value.
    last: Option<ScreenStatus>,
}

impl ScreenLatch {
    /// Fresh latch: nothing raised, nothing latched.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one polled grid, returning the status the poller may announce.
    ///
    /// Only transitions matter to the journal; the caller should diff against
    /// its own previous announcement exactly as the stateless path did.
    pub fn update(&mut self, grid: &ScreenGrid, hooks: HookHealth) -> Option<ScreenStatus> {
        let raw = screen_status(grid);
        let osc = osc::osc_status(&grid.osc);
        if raw == Some(ScreenStatus::Blocked) {
            self.blocked = true;
            // Never downgrade the source: a frame that carries both the title
            // edge and the dialog phrase takes the stricter release rule.
            self.osc_blocked |= osc.needs_input;
        }
        let mut status = raw;
        if self.blocked {
            let positive_idle = match (status, self.osc_blocked) {
                // An OSC-latched dialog ends on the explicit progress-off
                // edge; a phrase-latched dialog ends when the phrase is gone
                // and the composer is back (the pre-OSC text path).
                (Some(ScreenStatus::Idle), true) => osc.progress == Some(osc::STATE_OFF),
                (Some(ScreenStatus::Idle), false) => true,
                _ => false,
            };
            match status {
                Some(ScreenStatus::Working) | Some(ScreenStatus::Idle) if positive_idle => {
                    self.blocked = false;
                    self.osc_blocked = false;
                }
                Some(ScreenStatus::Working) => {
                    // Working without the explicit idle edge still proves the
                    // dialog answered: the turn resumed.
                    self.blocked = false;
                    self.osc_blocked = false;
                }
                Some(ScreenStatus::Blocked) => {}
                Some(ScreenStatus::Idle) | None => status = Some(ScreenStatus::Blocked),
            }
        }
        // Raise-only (rule 6): a low tier's busy edge is honoured, but its
        // idle edge is suppressed while the hook tier is alive. The held
        // value stays Working; a hook `Stop` idles the instance on its own
        // higher-tier channel regardless of what this latch reports.
        if status == Some(ScreenStatus::Idle) && self.raised && hooks.alive() {
            status = Some(ScreenStatus::Working);
        }
        match status {
            Some(ScreenStatus::Working) | Some(ScreenStatus::Blocked) => self.raised = true,
            Some(ScreenStatus::Idle) => self.raised = false,
            None => {}
        }
        self.last = status;
        status
    }

    /// The last verdict [`Self::update`] returned.
    #[must_use]
    pub fn last(&self) -> Option<ScreenStatus> {
        self.last
    }
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
    use crate::grid::OscState;
    use ScreenStatus::*;

    fn raw(screen: &str) -> ScreenGrid {
        ScreenGrid::from_raw(screen)
    }

    fn grid_with(screen: &str, osc: OscState) -> ScreenGrid {
        let mut grid = ScreenGrid::from_lines(screen.lines());
        grid.osc = osc;
        grid
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
    fn the_outside_reads_dialog_reads_as_blocked() {
        // dispatch-onboarding-1: the auto-mode outside-reads first-run modal
        // must not read as working even with the composer glyph on screen.
        let screen = format!(
            "\u{276f} probe\n{}",
            include_str!("../../remuda-testing/tests/fixtures/claude-outside-reads-dialog.txt")
        );
        assert_eq!(screen_status(&raw(&screen)), Some(ScreenStatus::Blocked));
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

    #[test]
    fn osc_busy_beats_the_prompt_glyph_without_any_footer_phrase() {
        // D-2: 2.1.270's working row contains no `esc to interrupt`, and the
        // composer glyph stays on screen throughout.
        let screen = "\u{276f} run the probe\n\u{273b} Grooving… (14s · ↓ 103 tokens)\n";
        let legacy = screen_status(&raw(screen));
        assert_eq!(
            legacy,
            Some(ScreenStatus::Idle),
            "the defect on main: text-only evidence says idle mid-turn"
        );
        let fixed = grid_with(
            screen,
            OscState {
                title: Some("\u{25d0} bash probe".into()),
                progress: Some("3".into()),
            },
        );
        assert_eq!(screen_status(&fixed), Some(ScreenStatus::Working));
    }

    #[test]
    fn the_spark_and_busy_progress_read_as_blocked_not_idle() {
        let screen = "\u{276f} run\nDo you want to run this command?\n";
        let grid = grid_with(
            screen,
            OscState {
                title: Some("\u{2733} bash probe".into()),
                progress: Some("3;".into()),
            },
        );
        assert_eq!(screen_status(&grid), Some(ScreenStatus::Blocked));
    }

    #[test]
    fn progress_off_with_the_composer_reads_as_idle() {
        let screen = "\u{276f} next?\n";
        let grid = grid_with(
            screen,
            OscState {
                title: Some("\u{2733} bash probe".into()),
                progress: Some("0;".into()),
            },
        );
        assert_eq!(screen_status(&grid), Some(ScreenStatus::Idle));
    }

    #[test]
    fn determinate_progress_vetoes_the_prompt_glyph_without_asserting_idle() {
        // Other harnesses (grok) emit 1;-1 while working: known, not idle.
        let screen = "\u{276f} draft\n";
        let grid = grid_with(
            screen,
            OscState {
                title: None,
                progress: Some("1;-1".into()),
            },
        );
        assert_eq!(screen_status(&grid), None);
    }

    #[test]
    fn blocked_latches_across_a_dropped_frame() {
        let dialog = grid_with(
            "\u{276f}\nDo you want to run this command?\n",
            OscState {
                title: Some("\u{2733} probe".into()),
                progress: Some("3".into()),
            },
        );
        // The blur frame: title repaint lost, no text rule on this row.
        let dropped = ScreenGrid::from_lines(["\u{276f}"]);
        let mut latch = ScreenLatch::new();
        assert_eq!(latch.update(&dialog, HookHealth::Healthy), Some(Blocked));
        assert_eq!(
            latch.update(&dropped, HookHealth::Healthy),
            Some(Blocked),
            "a dropped frame must not release the blocked latch"
        );
        // A positive working edge releases it.
        let working = grid_with(
            "\u{273b} Working…\n",
            OscState {
                title: Some("\u{25d0} probe".into()),
                progress: Some("3".into()),
            },
        );
        assert_eq!(latch.update(&working, HookHealth::Healthy), Some(Working));
    }

    #[test]
    fn osc_idle_does_not_lower_busy_while_the_hook_tier_is_healthy() {
        let mut latch = ScreenLatch::new();
        let working = grid_with(
            "\u{276f} p\n\u{273b} Working…\n",
            OscState {
                title: Some("\u{25d0} probe".into()),
                progress: Some("3".into()),
            },
        );
        let idle = grid_with(
            "\u{276f} \n",
            OscState {
                title: Some("\u{2733} probe".into()),
                progress: Some("0".into()),
            },
        );
        assert_eq!(latch.update(&working, HookHealth::Healthy), Some(Working));
        assert_eq!(
            latch.update(&idle, HookHealth::Healthy),
            Some(Working),
            "rule 6: OSC may raise busy, never clear it with hooks alive"
        );
        assert_eq!(latch.last(), Some(Working));
    }

    #[test]
    fn osc_idle_may_lower_busy_when_hooks_never_materialised() {
        let mut latch = ScreenLatch::new();
        let working = grid_with(
            "\u{273b} Working…\n",
            OscState {
                title: None,
                progress: Some("3".into()),
            },
        );
        let idle = grid_with(
            "\u{276f} \n",
            OscState {
                title: None,
                progress: Some("0".into()),
            },
        );
        assert_eq!(
            latch.update(&working, HookHealth::NeverMaterialised),
            Some(Working)
        );
        assert_eq!(
            latch.update(&idle, HookHealth::NeverMaterialised),
            Some(Idle),
            "the escape hatch: without a hook tier the screen is the best \
             authority and must be allowed to end the turn"
        );
    }

    #[test]
    fn a_stalled_hook_tier_is_treated_like_never_materialised_for_lowering() {
        let mut latch = ScreenLatch::new();
        let working = grid_with(
            "\u{273b} Working…\n",
            OscState {
                title: None,
                progress: Some("3".into()),
            },
        );
        let idle = grid_with(
            "\u{276f} \n",
            OscState {
                title: None,
                progress: Some("0".into()),
            },
        );
        latch.update(&working, HookHealth::Stalled);
        assert_eq!(latch.update(&idle, HookHealth::Stalled), Some(Idle));
    }

    #[test]
    fn idle_before_any_busy_edge_passes_through_under_healthy_hooks() {
        // The very first poll shows the ready composer and nothing has raised
        // busy: raise-only must not invent a working turn.
        let mut latch = ScreenLatch::new();
        let idle = raw("\u{276f} ask me anything\n");
        assert_eq!(latch.update(&idle, HookHealth::Healthy), Some(Idle));
    }

    #[test]
    fn an_osc_latched_dialog_releases_on_the_progress_off_edge_only() {
        let mut latch = ScreenLatch::new();
        let dialog = grid_with(
            "\u{276f}\n",
            OscState {
                title: Some("\u{2733} probe".into()),
                progress: Some("3".into()),
            },
        );
        let composer = grid_with(
            "\u{276f} \n",
            OscState {
                title: Some("\u{2733} probe".into()),
                progress: Some("3".into()),
            },
        );
        assert_eq!(latch.update(&dialog, HookHealth::Healthy), Some(Blocked));
        // Glyph present while progress still says busy: not a release.
        assert_eq!(latch.update(&composer, HookHealth::Healthy), Some(Blocked));
        let released = grid_with(
            "\u{276f} \n",
            OscState {
                title: Some("\u{2733} probe".into()),
                progress: Some("0".into()),
            },
        );
        // Healthy hooks turn the release into held-busy (rule 6).
        assert_eq!(latch.update(&released, HookHealth::Healthy), Some(Working));
    }

    #[test]
    fn a_phrase_latched_dialog_releases_when_the_phrase_is_gone() {
        // The pre-OSC text path: no OSC regions, the trust dialog phrase is
        // the whole evidence, and its disappearance plus the composer glyph
        // is the positive idle edge (older builds emit no 9;4;0).
        let mut latch = ScreenLatch::new();
        let dialog = raw(
            "Quick safety check:\nIs this a project you created or one you trust?\n\u{276f} Yes, I trust this folder\n",
        );
        let composer = raw("\u{276f} ready\n");
        assert_eq!(
            latch.update(&dialog, HookHealth::NeverMaterialised),
            Some(Blocked)
        );
        assert_eq!(
            latch.update(&composer, HookHealth::NeverMaterialised),
            Some(Idle)
        );
    }
}
