//! The rendered grid: this crate's real input (D-028 §4.1).
//!
//! The pre-D-028 matchers read an ANSI-stripped byte tail, which drops cursor
//! motion instead of replaying it — a full-screen TUI that repaints in place
//! therefore accumulated every past frame in the text the matchers saw. A
//! [`ScreenGrid`] is what the terminal actually shows: one string per visible
//! row, the cursor, and the mode flags the emulator observed.
//!
//! [`ScreenGrid::from_raw`] builds the degraded form from a byte tail so the
//! same matchers run whether or not the emulator is on; `emulated` records
//! which of the two a grid came from, because a matcher that wants to trust
//! line geometry may only do so on a real one.

/// DECSET modes the matchers and the snapshot repaint care about.
///
/// `?2004` gates bracketed paste (D-028 §5.2), `?1049` gates the alt-screen
/// snapshot and the web's wheel handling (§4.6), `?25` is cursor visibility,
/// and the mouse modes are reported so the repaint can restore them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModeSet {
    /// `?1049`: the alternate screen is active (full-screen TUI).
    pub alt_screen: bool,
    /// `?2004`: the application asked for bracketed paste.
    pub bracketed_paste: bool,
    /// `?25` is *reset*: the cursor is hidden.
    pub hide_cursor: bool,
    /// `?1000` / `?1002` / `?1003`: any mouse tracking mode is on.
    pub mouse_tracking: bool,
    /// `?1006` (or `?1005`): an extended mouse encoding is selected.
    pub mouse_sgr: bool,
    /// Application cursor keys (DECCKM).
    pub application_cursor: bool,
    /// Application keypad.
    pub application_keypad: bool,
}

/// Retained OSC payloads. The rule table reads these as regions (§10), so the
/// emulator must keep them rather than render and discard them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OscState {
    /// `OSC 0` / `OSC 2` window title.
    pub title: Option<String>,
    /// Raw `OSC 9;4` progress payload, e.g. `3;0` — state and percent.
    pub progress: Option<String>,
}

/// A rendered terminal screen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenGrid {
    /// Visible rows, top to bottom, with trailing blanks trimmed per row.
    pub lines: Vec<String>,
    /// Cursor position as `(row, col)`, zero-based, within `lines`.
    pub cursor: (u16, u16),
    /// Observed DECSET modes.
    pub modes: ModeSet,
    /// Retained OSC payloads.
    pub osc: OscState,
    /// True when a terminal emulator produced this grid. False means the grid
    /// was reconstructed from an ANSI-stripped byte tail and its line geometry
    /// is only as good as the escape stripper.
    pub emulated: bool,
}

impl ScreenGrid {
    /// Degraded grid from raw PTY bytes, for the emulator-off path.
    ///
    /// Escapes are stripped and the result is split on newlines. Cursor and
    /// modes are unknown, so they stay at their defaults and `emulated` is
    /// false — a caller that needs geometry must check that flag rather than
    /// assume one line here is one row on screen.
    #[must_use]
    pub fn from_raw(screen: &str) -> Self {
        let text = crate::ansi::strip_ansi(screen);
        Self {
            lines: text
                .lines()
                .map(|line| line.trim_end().to_owned())
                .collect(),
            cursor: (0, 0),
            modes: ModeSet::default(),
            osc: OscState::default(),
            emulated: false,
        }
    }

    /// Grid from already-rendered lines (tests, fixtures, future rule table).
    #[must_use]
    pub fn from_lines<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            lines: lines.into_iter().map(Into::into).collect(),
            cursor: (0, 0),
            modes: ModeSet::default(),
            osc: OscState::default(),
            emulated: true,
        }
    }

    /// The grid as text, one row per line.
    ///
    /// This is the matchers' working form. On an emulated grid it is the
    /// screen; on a raw one it is the stripped tail with its original breaks.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The grid as one whitespace-collapsed line.
    ///
    /// Two-token `contains` matches break across a soft wrap, so every phrase
    /// match runs against this rather than the raw text (§10 anchor ⑤).
    #[must_use]
    pub fn flat(&self) -> String {
        self.text().split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Last `n` rows that carry any non-whitespace, in screen order.
    ///
    /// `bottom_non_empty_lines(N)` from the §10 region vocabulary. A repainted
    /// TUI pads its screen with blank rows, so "the last N lines" and "the last
    /// N lines with something on them" are very different regions.
    #[must_use]
    pub fn bottom_non_empty_lines(&self, n: usize) -> Vec<&str> {
        let mut rows: Vec<&str> = self
            .lines
            .iter()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(n)
            .map(String::as_str)
            .collect();
        rows.reverse();
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_raw_grid_is_stripped_split_and_marked_non_emulated() {
        let grid = ScreenGrid::from_raw("\u{1b}[1;36mhead\u{1b}[0m  \r\nbody\r\n");
        assert_eq!(grid.lines, vec!["head", "body"]);
        assert!(
            !grid.emulated,
            "a stripped byte tail is not a rendered grid"
        );
        assert_eq!(grid.modes, ModeSet::default());
    }

    #[test]
    fn flat_collapses_the_soft_wrap_that_would_split_a_two_token_match() {
        let grid = ScreenGrid::from_lines(["Do you want to", "  proceed?"]);
        assert!(grid.flat().contains("Do you want to proceed?"));
        assert!(!grid.text().contains("Do you want to proceed?"));
    }

    #[test]
    fn bottom_non_empty_lines_skips_the_padding_a_tui_repaint_leaves() {
        let grid = ScreenGrid::from_lines(["one", "two", "", "three", "", ""]);
        assert_eq!(grid.bottom_non_empty_lines(2), vec!["two", "three"]);
        assert_eq!(grid.bottom_non_empty_lines(9), vec!["one", "two", "three"]);
    }
}
