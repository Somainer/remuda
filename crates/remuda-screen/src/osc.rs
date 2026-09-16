//! Reading the OSC tier the emulator already retains (design §2.4, D-3).
//!
//! `Emulator` captures `OSC 0`/`OSC 2` titles and `OSC 9;4` progress payloads
//! onto every [`ScreenGrid`], but until this module nothing read them. Real
//! claude 2.1.270 turns toggle both at the turn edges, measured 16–31 ms after
//! Enter (`claude-channels.md` §3.1/§3.2):
//!
//! ```text
//! ESC ] 0 ; ✳ <title> BEL      title glyph: ✳ idle/needs-input, ◐/◑ busy
//! ESC ] 9 ; 4 ; 3 ; BEL        progress state 3 (indeterminate = busy)
//! ESC ] 9 ; 4 ; 0 ; BEL        progress state 0 (off = idle)
//! ```
//!
//! Two traps the parser must keep (the probes caught both):
//!
//! - **The percent field is empty.** claude sends `9;4;3;`, while the
//!   emulator's own test feeds `9;4;3;0`. Match the *state token*, never the
//!   whole payload (`harness-parity.md` §2.1).
//! - **`✳` is ambiguous.** It marks both plain idle and a permission dialog.
//!   Disambiguate with progress still at state 3: `✳` + busy = needs input,
//!   `✳` alone = idle (`claude-channels.md` §0 row 5).

use crate::grid::OscState;

/// `✳` U+2733 — idle, or waiting for the user at a dialog. Ambiguous alone.
pub const GLYPH_NEEDS_INPUT: char = '\u{2733}';
/// `◐` U+25D0 — busy half-circle (alternates with [`GLYPH_BUSY_ALT`]).
pub const GLYPH_BUSY: char = '\u{25D0}';
/// `◑` U+25D1 — busy half-circle, alternate animation frame.
pub const GLYPH_BUSY_ALT: char = '\u{25D1}';

/// ConEmu OSC 9;4 progress state 0: no operation in progress.
pub const STATE_OFF: u8 = 0;
/// ConEmu OSC 9;4 progress state 3: indeterminate progress (claude's busy bit).
pub const STATE_INDETERMINATE: u8 = 3;

/// What the retained OSC regions say about the TUI right now.
///
/// Ephemeral status only — OSC never carries content (design §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OscStatus {
    /// Progress state token from `OSC 9;4`, e.g. `Some(3)` for `9;4;3;`.
    /// `None` when the harness never emitted the sequence.
    pub progress: Option<u8>,
    /// The turn is busy: indeterminate progress, or a busy title glyph.
    pub busy: bool,
    /// A native dialog owns the keyboard: the ambiguous `✳` glyph while
    /// progress still asserts busy.
    pub needs_input: bool,
    /// The title glyph, when the title begins with a recognised one.
    pub glyph: Option<char>,
}

/// Split the `OSC 9;4` state token off a retained payload.
///
/// Payloads seen in the wild are `"3"` (claude's empty-percent `9;4;3;`),
/// `"3;0"` and `"0;0"`. Only the first semicolon-delimited token is the state;
/// the remainder is a percent the sender may leave blank.
#[must_use]
pub fn progress_state(progress: &str) -> Option<u8> {
    progress
        .split(';')
        .next()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .and_then(|token| token.parse::<u8>().ok())
}

/// The recognised status glyph at the start of an `OSC 0` title, if any.
///
/// Titles arrive as `<glyph> <space> <session title>`, with a leading space in
/// some captures, so leading whitespace is skipped before taking the first
/// `char`.
#[must_use]
pub fn title_glyph(title: &str) -> Option<char> {
    let first = title.trim_start().chars().next()?;
    matches!(first, GLYPH_NEEDS_INPUT | GLYPH_BUSY | GLYPH_BUSY_ALT).then_some(first)
}

/// Interpret the emulator's retained OSC state.
#[must_use]
pub fn osc_status(osc: &OscState) -> OscStatus {
    let progress = osc.progress.as_deref().and_then(progress_state);
    let glyph = osc.title.as_deref().and_then(title_glyph);
    let indeterminate = progress == Some(STATE_INDETERMINATE);
    let busy_glyph = matches!(glyph, Some(GLYPH_BUSY | GLYPH_BUSY_ALT));
    OscStatus {
        progress,
        // Progress is authoritative for busy; the glyph covers the few
        // milliseconds between the title flip and the progress sequence.
        busy: indeterminate || busy_glyph,
        // `✳` alone means idle. Only `✳` while busy is a dialog.
        needs_input: indeterminate && glyph == Some(GLYPH_NEEDS_INPUT),
        glyph,
    }
}

/// Whether the progress token asserts an active operation (states 1–4).
///
/// State 0 and "never emitted" permit an idle read; any other token means the
/// bar knows work is in progress, so the prompt glyph alone must not classify
/// the screen as idle.
#[must_use]
pub fn progress_active(status: &OscStatus) -> bool {
    matches!(status.progress, Some(state) if state != STATE_OFF)
}

/// ConEmu `OSC 9;4` progress, parsed for the web terminal header.
///
/// The sequence is `OSC 9 ; 4 ; <state> ; <percent> ST`, with the percent
/// frequently left empty (claude emits `9;4;3;` while a turn runs). The
/// wire spelling is additive: older clients ignore the new notice, exactly
/// like the `altScreen` field it rides with (native-config, 2026-09-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressBar {
    /// State `0`: no operation in progress — the bar is hidden.
    Done,
    /// State `1`: determinate progress, with the 0..=100 percent when the
    /// sender gave one.
    Percent(Option<u8>),
    /// State `2`: the operation errored.
    Error,
    /// State `3`: indeterminate activity (claude's busy indicator).
    Indeterminate,
    /// State `4`: paused / warning.
    Paused,
}

impl ProgressBar {
    /// Parse a retained `OSC 9;4` payload.
    ///
    /// `None` means the harness never emitted a recognisable sequence, not
    /// "done": a fresh attach must not flash the bar just to hide it. Payloads
    /// seen in the wild are `"3"` (claude's empty-percent `9;4;3;`),
    /// `"3;0"`, `"1;50"` and `"0;0"`.
    #[must_use]
    pub fn parse(payload: &str) -> Option<Self> {
        let mut parts = payload.split(';').map(str::trim);
        let state: u8 = parts
            .next()
            .filter(|token| !token.is_empty())?
            .parse()
            .ok()?;
        let percent = parts
            .next()
            .filter(|token| !token.is_empty())
            .and_then(|token| token.parse::<u8>().ok())
            .map(|value| value.min(100));
        Some(match state {
            STATE_OFF => Self::Done,
            STATE_PERCENT => Self::Percent(percent),
            STATE_ERROR => Self::Error,
            STATE_INDETERMINATE => Self::Indeterminate,
            STATE_PAUSED => Self::Paused,
            _ => return None,
        })
    }

    /// Stable lowercase spelling for the additive `progress.state` wire field.
    #[must_use]
    pub fn state_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Percent(_) => "percent",
            Self::Error => "error",
            Self::Indeterminate => "indeterminate",
            Self::Paused => "paused",
        }
    }

    /// Reported percent for determinate progress.
    #[must_use]
    pub fn percent(self) -> Option<u8> {
        match self {
            Self::Percent(percent) => percent,
            _ => None,
        }
    }
}

/// ConEmu OSC 9;4 progress state 1: determinate percent.
pub const STATE_PERCENT: u8 = 1;
/// ConEmu OSC 9;4 progress state 2: error.
pub const STATE_ERROR: u8 = 2;
/// ConEmu OSC 9;4 progress state 4: paused / warning.
pub const STATE_PAUSED: u8 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_token_matches_with_an_empty_percent() {
        // Exact bytes from the 2.1.270 probe: `ESC ] 9 ; 4 ; 3 ; BEL`.
        assert_eq!(progress_state("3"), Some(3));
        assert_eq!(progress_state("3;0"), Some(3));
        assert_eq!(progress_state("0;0"), Some(0));
        assert_eq!(progress_state(""), None);
        assert_eq!(progress_state(";"), None);
        assert_eq!(progress_state("x;50"), None);
    }

    #[test]
    fn indeterminate_progress_is_busy_whatever_follows() {
        for payload in ["3", "3;0", "3;"] {
            let status = osc_status(&OscState {
                title: None,
                progress: Some(payload.into()),
            });
            assert!(status.busy, "{payload:?} must read as busy");
            assert!(!status.needs_input, "a glyph is required for needs-input");
        }
    }

    #[test]
    fn the_spark_is_needs_input_only_while_busy() {
        let dialog = osc_status(&OscState {
            title: Some("✳ Bash tool probe".into()),
            progress: Some("3".into()),
        });
        assert!(dialog.needs_input);
        assert!(dialog.busy);

        let idle = osc_status(&OscState {
            title: Some("✳ Bash tool probe".into()),
            progress: Some("0".into()),
        });
        assert!(!idle.needs_input, "✳ with progress off is plain idle");
        assert!(!idle.busy);
    }

    #[test]
    fn busy_title_glyphs_count_even_before_progress_arrives() {
        for glyph in [GLYPH_BUSY, GLYPH_BUSY_ALT] {
            let status = osc_status(&OscState {
                title: Some(format!("{glyph} Claude Code")),
                progress: None,
            });
            assert!(status.busy, "{glyph} is the busy title edge");
        }
    }

    #[test]
    fn no_osc_regions_is_unknown_not_idle() {
        assert_eq!(osc_status(&OscState::default()), OscStatus::default());
        assert!(!progress_active(&osc_status(&OscState::default())));
        assert!(progress_active(&osc_status(&OscState {
            title: None,
            progress: Some("1;-1".into())
        })));
    }

    #[test]
    fn osc_9_4_payloads_parse_into_header_states() {
        // The four states the terminal header renders, with claude's exact
        // empty-percent spelling (`ESC ] 9 ; 4 ; 3 ; BEL`).
        assert_eq!(ProgressBar::parse("3"), Some(ProgressBar::Indeterminate));
        assert_eq!(ProgressBar::parse("3;0"), Some(ProgressBar::Indeterminate));
        assert_eq!(ProgressBar::parse("3;"), Some(ProgressBar::Indeterminate));
        assert_eq!(
            ProgressBar::parse("1;50"),
            Some(ProgressBar::Percent(Some(50)))
        );
        assert_eq!(ProgressBar::parse("1;"), Some(ProgressBar::Percent(None)));
        assert_eq!(ProgressBar::parse("1;200"), Some(ProgressBar::Percent(Some(100))));
        assert_eq!(ProgressBar::parse("2;0"), Some(ProgressBar::Error));
        assert_eq!(ProgressBar::parse("4;10"), Some(ProgressBar::Paused));
        assert_eq!(ProgressBar::parse("0"), Some(ProgressBar::Done));
        assert_eq!(ProgressBar::parse("0;0"), Some(ProgressBar::Done));
        // Never-emitted / unparseable is None, which is distinct from Done:
        // an attach with no OSC evidence must clear a stale bar, not render one.
        assert_eq!(ProgressBar::parse(""), None);
        assert_eq!(ProgressBar::parse(";50"), None);
        assert_eq!(ProgressBar::parse("x;50"), None);
        assert_eq!(ProgressBar::parse("9;10"), None);
    }

    #[test]
    fn progress_state_spellings_and_percents_are_stable() {
        assert_eq!(ProgressBar::Done.state_str(), "done");
        assert_eq!(
            ProgressBar::Percent(Some(42)).state_str(),
            "percent"
        );
        assert_eq!(ProgressBar::Error.state_str(), "error");
        assert_eq!(ProgressBar::Indeterminate.state_str(), "indeterminate");
        assert_eq!(ProgressBar::Paused.state_str(), "paused");
        assert_eq!(ProgressBar::Percent(Some(42)).percent(), Some(42));
        assert_eq!(ProgressBar::Indeterminate.percent(), None);
    }
}
