//! Terminal emulator over the PTY byte stream (D-028 §4.1, §4.6).
//!
//! One [`Emulator`] per live PTY. Every byte the PTY produces is fed here *in
//! addition to* the raw ring — the ring stays the honest fallback, and an
//! emulator error must never cost a byte of terminal output.
//!
//! What it buys, per §4.1: a rendered grid instead of an ANSI-stripped tail
//! (so a repainting TUI stops accumulating dead frames), observed DECSET modes
//! (`?1049`, `?2004`, `?25`, mouse) instead of replaying stale ones out of the
//! ring, retained OSC 0/2 and 9;4 payloads for the §10 rule table, and a
//! synthesized repaint for the attach snapshot.
//!
//! ## Why vt100
//!
//! See `docs/design/evidence/native-pty-0.md` for the full comparison. In
//! short: `vt100` is the only one of the three candidates that ships all of
//! grid→ANSI re-render (`state_formatted`), configurable scrollback, alt-screen
//! state, DECSET tracking and OSC callbacks in a 3-dependency crate.
//! `wezterm-term` is not published to crates.io at all, and
//! `alacritty_terminal` pulls a PTY/event-loop stack we already own and has no
//! grid→ANSI renderer, which is exactly the piece §4.6's snapshot needs.

use crate::grid::{ModeSet, OscState, ScreenGrid};

/// Scrollback lines retained per emulator.
///
/// Chosen against the benchmark in `docs/design/evidence/native-pty-0.md`:
/// vt100 allocates every scrollback row eagerly at the terminal's width, so
/// cost is `rows × cols × 32 B`. At 1000 lines and a 200-column terminal that
/// is ~7.3 MiB per emulator and ~234 MiB across `maxInstances` = 32 — inside
/// the 256 MiB budget with the width headroom a wide window needs. 2000 lines
/// would be ~458 MiB at the same width, which is over.
///
/// This is the emulator's own scrollback, not the user's: the browser keeps
/// 4000 lines of its own and the byte ring is unchanged, so the cap only
/// bounds how far back a *server-side* rule or repaint can see.
pub const DEFAULT_SCROLLBACK_LINES: usize = 1000;

/// Columns beyond which the emulator refuses to grow, so one very wide client
/// cannot blow the per-instance budget the cap above was sized for.
pub const MAX_COLS: u16 = 400;

/// Rows beyond which the emulator refuses to grow.
pub const MAX_ROWS: u16 = 200;

/// Retained OSC payloads, collected through vt100's callback hooks.
///
/// vt100 renders and discards OSC by default; §10 needs `osc_title` and
/// `osc_progress` as matchable regions, so they are captured here.
#[derive(Debug, Default)]
struct OscSink {
    title: Option<String>,
    progress: Option<String>,
}

impl vt100::Callbacks for OscSink {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        // `OSC 9 ; 4 ; <state> ; <percent>` — the progress protocol claude and
        // agy emit. Keep the payload verbatim; interpreting it is §10's job.
        if let [b"9", rest @ ..] = params
            && let [b"4", payload @ ..] = rest
        {
            let joined = payload
                .iter()
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect::<Vec<_>>()
                .join(";");
            self.progress = Some(joined);
        }
    }
}

/// A terminal emulator fed from one PTY.
pub struct Emulator {
    parser: vt100::Parser<OscSink>,
    cols: u16,
    rows: u16,
    scrollback: usize,
}

impl std::fmt::Debug for Emulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emulator")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("scrollback", &self.scrollback)
            .finish_non_exhaustive()
    }
}

impl Emulator {
    /// New emulator at `cols × rows` with [`DEFAULT_SCROLLBACK_LINES`].
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_LINES)
    }

    /// New emulator with an explicit scrollback cap (benchmarks, tests).
    #[must_use]
    pub fn with_scrollback(cols: u16, rows: u16, scrollback: usize) -> Self {
        let cols = cols.clamp(1, MAX_COLS);
        let rows = rows.clamp(1, MAX_ROWS);
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, scrollback, OscSink::default()),
            cols,
            rows,
            scrollback,
        }
    }

    /// Feed PTY output bytes.
    ///
    /// Infallible by construction: vt100's parser has no error path, it just
    /// ignores what it does not understand. The fallible boundary is the
    /// caller's — see [`crate::SnapshotSource`].
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the emulated screen. Out-of-range sizes are clamped, not
    /// rejected: a clamped emulator still beats no emulator, and the raw ring
    /// is unaffected either way.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.clamp(1, MAX_COLS);
        let rows = rows.clamp(1, MAX_ROWS);
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Current size as `(cols, rows)`.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Scrollback lines retained.
    #[must_use]
    pub fn scrollback_lines(&self) -> usize {
        self.scrollback
    }

    /// `?1049` is active: a full-screen TUI owns the display.
    #[must_use]
    pub fn alt_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Observed DECSET modes.
    #[must_use]
    pub fn modes(&self) -> ModeSet {
        let screen = self.parser.screen();
        ModeSet {
            alt_screen: screen.alternate_screen(),
            bracketed_paste: screen.bracketed_paste(),
            hide_cursor: screen.hide_cursor(),
            mouse_tracking: screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None,
            mouse_sgr: screen.mouse_protocol_encoding() != vt100::MouseProtocolEncoding::Default,
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
        }
    }

    /// Retained OSC payloads.
    #[must_use]
    pub fn osc(&self) -> OscState {
        let sink = self.parser.callbacks();
        OscState {
            title: sink.title.clone(),
            progress: sink.progress.clone(),
        }
    }

    /// The rendered screen as a [`ScreenGrid`].
    ///
    /// This is what the §10 rule table and the signature matchers consume when
    /// the emulator is on. Only the *visible* rows are included: a matcher that
    /// looked into scrollback would re-classify dialogs the user scrolled past.
    #[must_use]
    pub fn grid(&self) -> ScreenGrid {
        let screen = self.parser.screen();
        let (row, col) = screen.cursor_position();
        ScreenGrid {
            lines: screen
                .rows(0, self.cols)
                .map(|line| line.trim_end().to_owned())
                .collect(),
            cursor: (row, col),
            modes: self.modes(),
            osc: self.osc(),
            emulated: true,
        }
    }

    /// Synthesized repaint for a fresh attach (§4.6).
    ///
    /// Reset, then the current screen, then the current mode set — rather than
    /// a slice of the byte ring, which starts mid-sequence, replays DECSETs the
    /// app has since turned off, and re-asks terminal queries whose asker is
    /// gone. When `?1049` is active this is the alt grid alone, which is what
    /// the caller wants: a full-screen TUI has no meaningful scrollback, and
    /// `alt_screen` on the attach tells the client to stop pretending it does.
    ///
    /// The leading `ESC[!p` (DECSTR, soft reset) clears whatever modes the
    /// client was left in without the screen-clearing side effects of RIS.
    /// `state_formatted` then paints the grid and re-asserts the modes it
    /// tracks; the mouse and `?1049` modes it does not re-emit are appended
    /// here.
    #[must_use]
    pub fn repaint(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut out = Vec::with_capacity(8192);
        out.extend_from_slice(b"\x1b[!p");
        if screen.alternate_screen() {
            // Put the client in the alternate buffer before painting, so the
            // repaint does not scribble the TUI's frame into the scrollback it
            // will return to when the app exits.
            out.extend_from_slice(b"\x1b[?1049h");
        } else {
            out.extend_from_slice(b"\x1b[?1049l");
        }
        out.extend_from_slice(&screen.state_formatted());
        // `state_formatted` covers bracketed paste, application cursor/keypad,
        // cursor visibility and the mouse protocol; alt-screen is handled
        // above. Nothing further to append today, but the ordering matters if
        // that ever changes: modes last, so a mode the paint clobbered is
        // restored rather than the other way round.
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(bytes: &[u8]) -> Emulator {
        let mut emulator = Emulator::new(40, 6);
        emulator.feed(bytes);
        emulator
    }

    #[test]
    fn the_grid_shows_the_repaint_not_the_history_that_produced_it() {
        // A TUI that redraws in place: the byte stream holds both frames, the
        // screen holds only the second. This is the whole reason for §4.1.
        let emulator = fed(b"\x1b[2J\x1b[Hfirst frame\x1b[2J\x1b[Hsecond frame");
        let grid = emulator.grid();
        assert_eq!(grid.lines[0], "second frame");
        assert!(
            !grid.text().contains("first frame"),
            "a stripped byte tail would still carry the dead frame"
        );
        assert!(grid.emulated);
    }

    #[test]
    fn decset_modes_are_observed_rather_than_replayed() {
        let emulator = fed(b"\x1b[?2004h\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h");
        let modes = emulator.modes();
        assert!(modes.bracketed_paste, "?2004 gates bracketed paste (§5.2)");
        assert!(modes.alt_screen);
        assert!(modes.hide_cursor);
        assert!(modes.mouse_tracking);
        assert!(modes.mouse_sgr);
        assert!(emulator.alt_screen());
    }

    #[test]
    fn a_mode_the_app_turned_off_is_off() {
        // The ring still contains the `h`; the emulator knows about the `l`.
        let emulator = fed(b"\x1b[?1000h\x1b[?1006h\x1b[?1000l\x1b[?1006l");
        let modes = emulator.modes();
        assert!(!modes.mouse_tracking);
        assert!(!modes.mouse_sgr);
    }

    #[test]
    fn osc_title_and_progress_are_retained_for_the_rule_table() {
        let emulator = fed(b"\x1b]0;claude - repo\x07\x1b]9;4;3;0\x07");
        let osc = emulator.osc();
        assert_eq!(osc.title.as_deref(), Some("claude - repo"));
        assert_eq!(osc.progress.as_deref(), Some("3;0"));
        // OSC 2 sets the title too.
        let emulator = fed(b"\x1b]2;later\x07");
        assert_eq!(emulator.osc().title.as_deref(), Some("later"));
    }

    #[test]
    fn a_repaint_reproduces_the_screen_in_a_fresh_emulator() {
        let emulator = fed(b"\x1b[2J\x1b[Hhello\r\n\x1b[1;31mred\x1b[0m\x1b[?2004h");
        let mut replayed = Emulator::new(40, 6);
        replayed.feed(&emulator.repaint());
        assert_eq!(replayed.grid().lines, emulator.grid().lines);
        assert!(
            replayed.modes().bracketed_paste,
            "the repaint must carry the mode set, not just the text"
        );
    }

    #[test]
    fn an_alt_screen_repaint_puts_the_client_in_the_alt_buffer_first() {
        let emulator = fed(b"scrollback line\r\n\x1b[?1049h\x1b[2J\x1b[HTUI frame");
        let repaint = emulator.repaint();
        let head = &repaint[..repaint.len().min(16)];
        assert!(
            head.windows(8).any(|w| w == b"\x1b[?1049h"),
            "alt-screen must be entered before the paint: {:?}",
            String::from_utf8_lossy(head)
        );
        let mut replayed = Emulator::new(40, 6);
        replayed.feed(&repaint);
        assert!(replayed.alt_screen());
        assert_eq!(replayed.grid().lines[0], "TUI frame");
        assert!(
            !replayed.grid().text().contains("scrollback line"),
            "alt-screen snapshots only the alt grid (§4.6)"
        );
    }

    #[test]
    fn leaving_the_alt_screen_restores_the_primary_grid() {
        let emulator = fed(b"primary\r\n\x1b[?1049h\x1b[2J\x1b[Halt\x1b[?1049l");
        assert!(!emulator.alt_screen());
        assert!(emulator.grid().text().contains("primary"));
        let repaint = emulator.repaint();
        assert!(
            repaint.windows(8).any(|w| w == b"\x1b[?1049l"),
            "a client left in the alt buffer must be brought back out"
        );
    }

    #[test]
    fn sizes_are_clamped_rather_than_rejected() {
        let mut emulator = Emulator::new(0, 0);
        assert_eq!(emulator.size(), (1, 1));
        emulator.resize(u16::MAX, u16::MAX);
        assert_eq!(emulator.size(), (MAX_COLS, MAX_ROWS));
    }

    #[test]
    fn scrollback_is_bounded_at_the_configured_cap() {
        let mut emulator = Emulator::with_scrollback(20, 4, 10);
        for i in 0..200 {
            emulator.feed(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(emulator.scrollback_lines(), 10);
        // The visible grid is still just the viewport.
        assert_eq!(emulator.grid().lines.len(), 4);
        assert!(emulator.grid().text().contains("line 199"));
    }
}
