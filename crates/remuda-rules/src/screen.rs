//! The engine's input: a rendered terminal grid plus the OSC payloads the VT
//! kept, and the structural helpers the regions are cut from.
//!
//! This is deliberately *not* a de-ANSI'd byte tail. D-028 §4.1 and §10 both
//! require the emulator grid: cursor motion that the byte tail would drop
//! silently shifts every signature, and the OSC title / `9;4` progress payloads
//! have to survive in VT state rather than being rendered and forgotten.

/// A rendered screen handed to the engine by the carrier.
///
/// Build one with [`Screen::new`] and the `with_*` setters; the carrier owns
/// `rows` (one entry per grid row, no trailing newline, no ANSI) and the two
/// OSC payloads.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    rows: Vec<String>,
    cursor: (usize, usize),
    osc_title: Option<String>,
    osc_progress: Option<String>,
    /// Rows after soft-wrap joining; see [`Screen::logical_rows`].
    logical: Vec<String>,
    /// For each logical row, the indices of the grid rows it came from. Lets a
    /// verdict quote the grid rows a joined line matched on.
    sources: Vec<Vec<usize>>,
    /// Terminal width the grid was rendered at, if the carrier pinned one.
    cols: Option<usize>,
}

impl Screen {
    /// Build a screen from rendered grid rows.
    ///
    /// Rows must be the emulator's rendered text: no ANSI, no trailing
    /// newline, one entry per grid row including blank ones (blank rows are
    /// what `bottom_non_empty_lines(N)` and the prompt-box search skip over).
    #[must_use]
    pub fn new(rows: Vec<String>) -> Self {
        let mut screen = Self {
            rows,
            ..Self::default()
        };
        screen.rewrap();
        screen
    }

    /// Set the cursor position as `(row, col)`, both zero-based.
    #[must_use]
    pub fn with_cursor(mut self, row: usize, col: usize) -> Self {
        self.cursor = (row, col);
        self
    }

    /// Set the OSC 0/2 window title the VT is currently holding.
    #[must_use]
    pub fn with_osc_title(mut self, title: impl Into<String>) -> Self {
        self.osc_title = Some(title.into());
        self
    }

    /// Set the OSC 9;4 progress payload, *without* the leading `9;`.
    ///
    /// herdr retains what follows `9;`, so the rules match on `4;1;-1` (busy)
    /// and `4;0;0` (idle) — pass it in that same shape.
    #[must_use]
    pub fn with_osc_progress(mut self, progress: impl Into<String>) -> Self {
        self.osc_progress = Some(progress.into());
        self
    }

    /// Pin the width the grid was rendered at, enabling soft-wrap joining.
    ///
    /// D-028 §10 anchor ⑤ says to pin `cols` at startup, and this is why: a
    /// row can only have soft-wrapped if it reached the right margin, and
    /// without the width there is no sound test for that. So joining happens
    /// only once a width is pinned — see [`Self::logical_rows`].
    #[must_use]
    pub fn with_cols(mut self, cols: usize) -> Self {
        self.cols = Some(cols);
        self.rewrap();
        self
    }

    /// The raw grid rows, one per terminal row.
    #[must_use]
    pub fn rows(&self) -> &[String] {
        &self.rows
    }

    /// Cursor position as `(row, col)`.
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    /// The OSC 0/2 title, if the VT has one.
    #[must_use]
    pub fn osc_title(&self) -> Option<&str> {
        self.osc_title.as_deref()
    }

    /// The OSC 9;4 progress payload, if the VT has one.
    #[must_use]
    pub fn osc_progress(&self) -> Option<&str> {
        self.osc_progress.as_deref()
    }

    /// Width the grid was rendered at, if pinned.
    #[must_use]
    pub fn cols(&self) -> Option<usize> {
        self.cols
    }

    /// Rows after soft-wrap joining — what every text region is cut from.
    ///
    /// D-028 §10 anchor ⑤: a narrow pane soft-wraps `do you want to proceed?`
    /// across two grid rows, and every two-token `contains` then misses. The
    /// engine matches on these joined rows instead.
    ///
    /// Joining requires a pinned width ([`Self::with_cols`]); without one this
    /// returns the grid rows unchanged, since there is no sound way to tell a
    /// wrapped row from a short one.
    #[must_use]
    pub fn logical_rows(&self) -> &[String] {
        &self.logical
    }

    /// The grid row indices that logical row `idx` was joined from.
    #[must_use]
    pub fn source_rows(&self, idx: usize) -> &[usize] {
        self.sources.get(idx).map_or(&[], Vec::as_slice)
    }

    /// Join soft-wrapped grid rows into logical rows.
    ///
    /// A row continues the one above when the row above ran to the margin and
    /// this row does not look like a line of its own. The join is
    /// character-exact — a terminal soft wrap neither adds nor removes
    /// anything, so `…want to` + ` proceed?` must come back as
    /// `…want to proceed?`. Trimming the continuation would weld the two words
    /// together and defeat the very `contains` this exists to rescue.
    ///
    /// "Looks like its own line" is the anchor-⑤ companion to anchor ①: claude
    /// renders activity summaries at column zero and indents wrapped
    /// continuations, so a row starting at column zero with a bullet, a
    /// box-drawing glyph, or a prompt marker is never folded into its
    /// predecessor — otherwise user prompt text could be glued onto a signal
    /// line and impersonate it.
    ///
    /// Note that app-level wrapping is excluded for free: an agent that wraps
    /// its own output emits a real newline *before* the margin, so the row
    /// above does not reach it and no join happens.
    fn rewrap(&mut self) {
        self.logical = Vec::with_capacity(self.rows.len());
        self.sources = Vec::with_capacity(self.rows.len());
        for (idx, row) in self.rows.iter().enumerate() {
            let prev_full = self
                .logical
                .last()
                .is_some_and(|prev: &String| self.reaches_margin(prev));
            if prev_full
                && !starts_new_line(row)
                && !row.trim().is_empty()
                && let (Some(prev), Some(src)) = (self.logical.last_mut(), self.sources.last_mut())
            {
                // Drop the predecessor's margin padding, then append the
                // continuation verbatim.
                prev.truncate(prev.trim_end().len());
                prev.push_str(row);
                src.push(idx);
                continue;
            }
            self.logical.push(row.clone());
            self.sources.push(vec![idx]);
        }
    }

    /// Whether `line` occupies the full width, i.e. could have wrapped.
    ///
    /// Trailing padding does not count. An emulator hands back rows padded out
    /// to the full width, so a naive length test would call every row a
    /// wrap candidate and glue the whole screen into one line. Only a row
    /// whose *content* reaches the margin can have wrapped.
    ///
    /// Answerable only with a pinned width; returning `false` when unpinned is
    /// what makes [`Self::logical_rows`] the identity in that case.
    fn reaches_margin(&self, line: &str) -> bool {
        self.cols
            .is_some_and(|cols| cols > 0 && line.trim_end().chars().count() >= cols)
    }
}

/// True when `row` starts something that is structurally its own line, so it
/// must not be folded onto the row above.
fn starts_new_line(row: &str) -> bool {
    let Some(first) = row.chars().next() else {
        return true;
    };
    // Leading whitespace is ambiguous: it is both how a wrapped continuation
    // of an indented paragraph looks and how the next indented UI row looks.
    // Decide on what follows it rather than on the indent itself.
    let trimmed = row.trim_start();
    if first == ' ' || first == '\t' {
        return starts_structural_glyph(trimmed);
    }
    starts_structural_glyph(row)
}

/// True when `text` opens with a glyph that only ever starts a fresh UI row.
fn starts_structural_glyph(text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return true;
    };
    if matches!(
        first,
        // Claude/codex/grok activity bullets and spinners (anchor ①).
        '*' | '\u{00B7}' | '\u{2022}' | '\u{25E6}' | '\u{2722}' | '\u{2736}' | '\u{273B}'
        | '\u{273D}' | '\u{25D0}'..='\u{25D3}' | '\u{2800}'..='\u{28FF}'
        // Prompt and selection markers.
        | '>' | '\u{276F}' | '\u{276D}' | '\u{203A}' | '\u{25B8}' | '\u{25B6}'
        // Box drawing and block elements: chrome, never wrapped prose.
        | '\u{2500}'..='\u{257F}' | '\u{2580}'..='\u{259F}'
    ) {
        return true;
    }
    // A numbered menu choice ("1. Yes", "2) No") is its own row.
    let mut chars = text.chars();
    if chars.next().is_some_and(|c| c.is_ascii_digit()) {
        let rest = chars
            .as_str()
            .trim_start_matches(|c: char| c.is_ascii_digit());
        if rest.starts_with(['.', ')']) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_soft_wrapped_rows_at_pinned_width() {
        let screen = Screen::new(vec![
            "Do you want to procee".to_owned(),
            "d?".to_owned(),
            "".to_owned(),
        ])
        .with_cols(21);
        assert_eq!(screen.logical_rows()[0], "Do you want to proceed?");
        assert_eq!(screen.source_rows(0), &[0, 1]);
    }

    #[test]
    fn keeps_column_zero_bullets_separate() {
        // Anchor ①: a bullet at column zero is its own line even when the row
        // above filled the margin, so user text cannot be glued onto a signal.
        let screen = Screen::new(vec![
            "some assistant output filling the row".to_owned(),
            "· Thinking…".to_owned(),
        ])
        .with_cols(37);
        assert_eq!(screen.logical_rows().len(), 2);
        assert_eq!(screen.logical_rows()[1], "· Thinking…");
    }

    #[test]
    fn short_rows_are_never_joined() {
        let screen = Screen::new(vec!["first".to_owned(), "second".to_owned()]).with_cols(80);
        assert_eq!(screen.logical_rows().len(), 2);
    }

    #[test]
    fn without_a_pinned_width_rows_pass_through() {
        let screen = Screen::new(vec!["Do you want to procee".to_owned(), "d?".to_owned()]);
        assert_eq!(screen.logical_rows(), ["Do you want to procee", "d?"]);
    }

    #[test]
    fn wrap_join_is_character_exact() {
        // A terminal soft wrap adds nothing and removes nothing. Trimming the
        // continuation would weld "to" onto "proceed?" and defeat the
        // `contains` this whole mechanism exists to rescue.
        let screen = Screen::new(vec![
            "Claude wants to run it. Do you want to".to_owned(),
            " proceed?                             ".to_owned(),
        ])
        .with_cols(38);
        assert_eq!(
            screen.logical_rows()[0].trim_end(),
            "Claude wants to run it. Do you want to proceed?"
        );
    }

    #[test]
    fn an_indented_menu_choice_is_not_a_continuation() {
        // `  ❯ 1. Yes` is indented, but it is a fresh UI row: folding it onto
        // the wrapped question above would hide it from the choice matchers.
        let screen = Screen::new(vec![
            "Claude wants to run it. Do you want to".to_owned(),
            " proceed?                             ".to_owned(),
            "  ❯ 1. Yes                            ".to_owned(),
            "    2. No                             ".to_owned(),
        ])
        .with_cols(38);
        assert_eq!(
            screen.logical_rows().len(),
            3,
            "{:?}",
            screen.logical_rows()
        );
        assert!(screen.logical_rows()[1].contains("❯ 1. Yes"));
        assert!(screen.logical_rows()[2].contains("2. No"));
    }
}
