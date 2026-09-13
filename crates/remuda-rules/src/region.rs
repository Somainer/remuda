//! Regions: the slice of the screen a rule's matchers are allowed to see.
//!
//! Every region here is one of the ten named in D-028 §10. Regions are cut
//! from [`Screen::logical_rows`], i.e. after soft-wrap joining, so a two-token
//! `contains` survives a narrow pane (anchor ⑤).

use std::str::FromStr;

use crate::error::Error;
use crate::screen::Screen;

/// Where a rule looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    /// The OSC 0/2 window title. Empty when the VT has none.
    OscTitle,
    /// The OSC 9;4 progress payload, minus the leading `9;`.
    OscProgress,
    /// The whole rendered grid.
    WholeRecent,
    /// The last `N` non-empty rows, back in screen order.
    BottomNonEmptyLines(usize),
    /// The first `N` non-empty rows. Engine 3 and up.
    TopNonEmptyLines(usize),
    /// Inside the prompt box the agent draws at the bottom of the screen.
    PromptBoxBody,
    /// Everything below the last horizontal rule.
    AfterLastHorizontalRule,
    /// Everything below the last prompt marker row.
    AfterLastPromptMarker,
    /// The single last non-empty row above the prompt box.
    LastNonEmptyAbovePromptBox,
    /// The whole grid minus the current prompt marker row and what follows.
    WholeRecentWithoutCurrentPromptMarker,
}

impl Region {
    /// Lowest engine level that implements this region.
    ///
    /// herdr refuses a rule that "uses `top_non_empty_lines` but
    /// `min_engine_version` is below" the level that introduced it, so a
    /// manifest cannot claim an older engine than its regions need.
    #[must_use]
    pub const fn min_engine_version(self) -> u32 {
        match self {
            Self::TopNonEmptyLines(_) => 3,
            _ => 1,
        }
    }

    /// The region's canonical TOML spelling.
    #[must_use]
    pub fn as_toml(self) -> String {
        match self {
            Self::OscTitle => "osc_title".to_owned(),
            Self::OscProgress => "osc_progress".to_owned(),
            Self::WholeRecent => "whole_recent".to_owned(),
            Self::BottomNonEmptyLines(n) => format!("bottom_non_empty_lines({n})"),
            Self::TopNonEmptyLines(n) => format!("top_non_empty_lines({n})"),
            Self::PromptBoxBody => "prompt_box_body".to_owned(),
            Self::AfterLastHorizontalRule => "after_last_horizontal_rule".to_owned(),
            Self::AfterLastPromptMarker => "after_last_prompt_marker".to_owned(),
            Self::LastNonEmptyAbovePromptBox => "last_non_empty_above_prompt_box".to_owned(),
            Self::WholeRecentWithoutCurrentPromptMarker => {
                "whole_recent_without_current_prompt_marker".to_owned()
            }
        }
    }

    /// Cut this region out of `screen`.
    ///
    /// An absent OSC payload yields no lines at all, so an OSC rule simply
    /// does not fire rather than matching the empty string.
    ///
    /// Lines come back as owned strings because [`Region::PromptBoxBody`]
    /// rewrites them (see [`strip_gutters`]); every other region passes the
    /// screen's logical rows through verbatim, which is what lets rules match
    /// box-drawing glyphs directly — grok's `option_dialog_blocked` anchors on
    /// `┃` and gemini's on `│ Apply this change`.
    #[must_use]
    pub fn extract(self, screen: &Screen) -> Vec<String> {
        let rows = screen.logical_rows();
        let owned = |s: &String| s.clone();
        match self {
            Self::OscTitle => screen.osc_title().map(str::to_owned).into_iter().collect(),
            Self::OscProgress => screen
                .osc_progress()
                .map(str::to_owned)
                .into_iter()
                .collect(),
            Self::WholeRecent => rows.iter().map(owned).collect(),
            Self::BottomNonEmptyLines(n) => {
                let mut picked: Vec<String> = rows
                    .iter()
                    .rev()
                    .filter(|r| !r.trim().is_empty())
                    .take(n)
                    .map(owned)
                    .collect();
                picked.reverse();
                picked
            }
            Self::TopNonEmptyLines(n) => rows
                .iter()
                .filter(|r| !r.trim().is_empty())
                .take(n)
                .map(owned)
                .collect(),
            // The only region that rewrites its lines: it exists to look
            // *inside* the composer, and the rules that use it are anchored
            // with `^\s*❯`, which the left border would otherwise block.
            Self::PromptBoxBody => prompt_box(rows).map_or_else(Vec::new, |b| {
                rows[b.body_start..b.body_end]
                    .iter()
                    .map(|r| strip_gutters(r))
                    .collect()
            }),
            Self::AfterLastHorizontalRule => match rows.iter().rposition(|r| is_horizontal_rule(r))
            {
                Some(idx) => rows[idx + 1..].iter().map(owned).collect(),
                None => rows.iter().map(owned).collect(),
            },
            Self::AfterLastPromptMarker => match rows.iter().rposition(|r| is_prompt_marker(r)) {
                Some(idx) => rows[idx + 1..].iter().map(owned).collect(),
                None => rows.iter().map(owned).collect(),
            },
            Self::LastNonEmptyAbovePromptBox => {
                let limit = prompt_box(rows).map_or(rows.len(), |b| b.start);
                rows[..limit]
                    .iter()
                    .rev()
                    .find(|r| !r.trim().is_empty())
                    .map(owned)
                    .into_iter()
                    .collect()
            }
            Self::WholeRecentWithoutCurrentPromptMarker => {
                match rows.iter().rposition(|r| is_prompt_marker(r)) {
                    Some(idx) => rows[..idx].iter().map(owned).collect(),
                    None => rows.iter().map(owned).collect(),
                }
            }
        }
    }
}

impl FromStr for Region {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parsed = |name: &str| -> Option<usize> {
            let rest = s.strip_prefix(name)?.strip_prefix('(')?.strip_suffix(')')?;
            rest.parse().ok()
        };
        match s {
            "osc_title" => return Ok(Self::OscTitle),
            "osc_progress" => return Ok(Self::OscProgress),
            "whole_recent" => return Ok(Self::WholeRecent),
            "prompt_box_body" => return Ok(Self::PromptBoxBody),
            "after_last_horizontal_rule" => return Ok(Self::AfterLastHorizontalRule),
            "after_last_prompt_marker" => return Ok(Self::AfterLastPromptMarker),
            "last_non_empty_above_prompt_box" => return Ok(Self::LastNonEmptyAbovePromptBox),
            "whole_recent_without_current_prompt_marker" => {
                return Ok(Self::WholeRecentWithoutCurrentPromptMarker);
            }
            _ => {}
        }
        if let Some(n) = parsed("bottom_non_empty_lines") {
            return Ok(Self::BottomNonEmptyLines(n));
        }
        if let Some(n) = parsed("top_non_empty_lines") {
            return Ok(Self::TopNonEmptyLines(n));
        }
        Err(Error::UnknownRegion(s.to_owned()))
    }
}

/// A prompt box located in the grid, as row index ranges.
struct PromptBox {
    /// Index of the top border row.
    start: usize,
    /// First body row, i.e. `start + 1`.
    body_start: usize,
    /// One past the last body row: the bottom border, or the grid end.
    body_end: usize,
}

/// Find the composer prompt box on the screen.
///
/// A prompt box is the bordered composer the agent draws at the bottom:
///
/// ```text
/// ╭──────────────────────────────╮
/// │ ❯ ask me something           │
/// ╰──────────────────────────────╯
/// ```
///
/// Being a bordered box is not enough — claude draws its permission dialogs in
/// exactly the same frame. The composer is identified by its body *starting*
/// with the prompt marker, which a dialog's body never does (it opens with a
/// header like `Bash command`, and its `❯ 1. Yes` is a selection cursor, which
/// [`is_prompt_marker`] already rejects). Without that test the priority-950
/// `live_prompt_box` rule would claim a permission dialog and report **idle**
/// while the agent is blocked waiting on a human.
///
/// Scanning from the bottom finds the live composer rather than one scrolled
/// up in the transcript.
fn prompt_box(rows: &[String]) -> Option<PromptBox> {
    let mut search_end = rows.len();
    while let Some(candidate) = locate_box(rows, search_end) {
        let body = &rows[candidate.body_start..candidate.body_end];
        let opens_with_marker = body
            .iter()
            .map(|r| strip_gutters(r))
            .find(|r| !r.trim().is_empty())
            .is_some_and(|first| is_prompt_marker(&first));
        if opens_with_marker {
            return Some(candidate);
        }
        // Not the composer; keep looking further up.
        search_end = candidate.start;
    }
    None
}

/// Locate the last bordered box that ends before `search_end`.
fn locate_box(rows: &[String], search_end: usize) -> Option<PromptBox> {
    let rows = &rows[..search_end.min(rows.len())];
    let bottom = rows.iter().rposition(|r| is_box_bottom(r));
    let top_limit = bottom.unwrap_or(rows.len());
    let start = rows[..top_limit].iter().rposition(|r| is_box_top(r))?;
    Some(PromptBox {
        start,
        body_start: start + 1,
        // An unterminated box (the bottom border scrolled off, or the pane cut
        // it) still has a usable body: everything below the top border.
        body_end: bottom.unwrap_or(rows.len()).max(start + 1),
    })
}

/// Strip a prompt box's left and right borders from a body row.
///
/// `│ ❯ hello        │` becomes `❯ hello`. Only used by
/// [`Region::PromptBoxBody`], whose rules anchor on `^\s*❯` and would never
/// match through the left border.
fn strip_gutters(row: &str) -> String {
    const GUTTERS: [char; 4] = ['\u{2502}', '\u{2503}', '\u{258C}', '\u{2590}'];
    let trimmed = row.trim();
    let inner = trimmed.strip_prefix(GUTTERS).unwrap_or(trimmed);
    let inner = inner.strip_suffix(GUTTERS).unwrap_or(inner);
    inner.trim().to_owned()
}

/// True for a row that is a horizontal rule: box-drawing or dashes only.
///
/// This is the divider claude draws above a permission dialog, which
/// `after_last_horizontal_rule` uses to isolate the dialog from the transcript
/// above it. A prompt-box border qualifies, which is intended — the region is
/// "below the last divider of any kind".
fn is_horizontal_rule(row: &str) -> bool {
    let trimmed = row.trim();
    if trimmed.chars().count() < 3 {
        return false;
    }
    trimmed.chars().all(|c| {
        matches!(
            c,
            '-' | '_'
                | '='
                | '\u{2500}'..='\u{257F}' // box drawing
                | '\u{2580}'..='\u{259F}' // block elements
                | '\u{22EF}' | '\u{2026}'
                | ' '
        )
    }) && trimmed.chars().any(|c| c != ' ')
}

/// True for a row whose first glyph is the composer's prompt marker.
///
/// Anchored at the start of the row so a `>` inside assistant prose cannot
/// pass for the composer. A marker followed by a numbered or lettered choice
/// (`❯ 1. Yes`, `❯ Yes, proceed`) is a *selection cursor* inside a dialog, not
/// a composer: treating it as a prompt marker would cut
/// `after_last_prompt_marker` below the dialog and hide the very rows codex's
/// `live_strong_blocker` needs to see.
fn is_prompt_marker(row: &str) -> bool {
    let rest = row.trim_start();
    // Strip the box gutter codex and grok draw to the left of the marker.
    let rest = rest
        .strip_prefix(['\u{2502}', '\u{2503}', '\u{258C}'])
        .map_or(rest, str::trim_start);
    let mut chars = rest.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(
        first,
        '>' | '\u{276F}' | '\u{276D}' | '\u{203A}' | '\u{25B8}' | '\u{25B6}'
    ) {
        return false;
    }
    !is_menu_choice(chars.as_str().trim_start())
}

/// True when `text` reads as a menu choice rather than composer input.
fn is_menu_choice(text: &str) -> bool {
    let lower = text.to_lowercase();
    // "1. Yes", "2) No" — a digit followed by a separator.
    let mut chars = lower.chars();
    if let Some(c) = chars.next()
        && c.is_ascii_digit()
    {
        let rest = chars
            .as_str()
            .trim_start_matches(|c: char| c.is_ascii_digit());
        if rest.starts_with(['.', ')', ':']) {
            return true;
        }
    }
    // The bare choice words a selection cursor sits on.
    [
        "yes", "no,", "no ", "allow", "deny", "accept", "decline", "trust", "don't",
    ]
    .iter()
    .any(|w| lower.starts_with(w))
        || lower == "no"
}

fn is_box_top(row: &str) -> bool {
    let t = row.trim();
    t.starts_with(['\u{256D}', '\u{250C}', '\u{2552}', '\u{2554}', '\u{250F}'])
        && is_horizontal_rule(t)
}

fn is_box_bottom(row: &str) -> bool {
    let t = row.trim();
    t.starts_with(['\u{2570}', '\u{2514}', '\u{2558}', '\u{255A}', '\u{2517}'])
        && is_horizontal_rule(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(lines: &[&str]) -> Screen {
        Screen::new(lines.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn parses_every_region_spelling() {
        for spelling in [
            "osc_title",
            "osc_progress",
            "whole_recent",
            "bottom_non_empty_lines(12)",
            "top_non_empty_lines(20)",
            "prompt_box_body",
            "after_last_horizontal_rule",
            "after_last_prompt_marker",
            "last_non_empty_above_prompt_box",
            "whole_recent_without_current_prompt_marker",
        ] {
            let region: Region = spelling.parse().expect("region parses");
            assert_eq!(region.as_toml(), spelling, "round trip");
        }
    }

    #[test]
    fn rejects_unknown_region() {
        assert!("sideways_lines(3)".parse::<Region>().is_err());
    }

    #[test]
    fn bottom_non_empty_lines_skips_blanks_and_keeps_order() {
        let s = screen(&["a", "", "b", "", "c", ""]);
        let got = Region::BottomNonEmptyLines(2).extract(&s);
        assert_eq!(got, vec!["b", "c"]);
    }

    #[test]
    fn top_non_empty_lines_needs_engine_three() {
        assert_eq!(Region::TopNonEmptyLines(1).min_engine_version(), 3);
        assert_eq!(Region::WholeRecent.min_engine_version(), 1);
    }

    #[test]
    fn absent_osc_payload_yields_no_lines() {
        let s = screen(&["hello"]);
        assert!(Region::OscTitle.extract(&s).is_empty());
        assert!(Region::OscProgress.extract(&s).is_empty());
    }

    #[test]
    fn prompt_box_body_strips_the_gutters() {
        let s = screen(&[
            "transcript line",
            "╭────────────────╮",
            "│ ❯ hello        │",
            "╰────────────────╯",
            "  footer hint",
        ]);
        // The border must be gone: `live_prompt_box` anchors on `^\s*❯`.
        assert_eq!(Region::PromptBoxBody.extract(&s), vec!["❯ hello"]);
        assert_eq!(
            Region::LastNonEmptyAbovePromptBox.extract(&s),
            vec!["transcript line"]
        );
    }

    #[test]
    fn other_regions_keep_box_glyphs_verbatim() {
        // grok's option_dialog_blocked anchors on `┃` and gemini's on
        // `│ Apply this change`, so only prompt_box_body may rewrite lines.
        let s = screen(&["┃  2 (○) Yes, proceed"]);
        assert_eq!(
            Region::WholeRecent.extract(&s),
            vec!["┃  2 (○) Yes, proceed"]
        );
    }

    #[test]
    fn after_last_horizontal_rule_falls_back_to_whole_screen() {
        let s = screen(&["only", "text"]);
        assert_eq!(
            Region::AfterLastHorizontalRule.extract(&s),
            vec!["only", "text"]
        );
    }

    #[test]
    fn prompt_marker_regions_split_around_the_marker() {
        let s = screen(&["above", "› ask", "below"]);
        assert_eq!(Region::AfterLastPromptMarker.extract(&s), vec!["below"]);
        assert_eq!(
            Region::WholeRecentWithoutCurrentPromptMarker.extract(&s),
            vec!["above"]
        );
    }

    #[test]
    fn angle_bracket_inside_prose_is_not_a_prompt_marker() {
        assert!(!is_prompt_marker("the value is > 3"));
        assert!(is_prompt_marker("  > ask"));
        assert!(is_prompt_marker("┃ ❯ ask"));
    }

    #[test]
    fn selection_cursor_is_not_a_prompt_marker() {
        // `❯ 1. Yes` is a dialog cursor. Treating it as the composer would cut
        // after_last_prompt_marker below the dialog and hide the choice rows.
        for row in [
            "❯ 1. Yes",
            "  ❯ Yes, proceed",
            "❯ 2. No, and tell codex what to do",
            "❯ Allow once",
            "❯ Decline",
        ] {
            assert!(!is_prompt_marker(row), "{row:?} is a selection cursor");
        }
    }

    #[test]
    fn after_last_prompt_marker_keeps_the_dialog_below_the_composer() {
        let s = screen(&[
            "› run the migration",
            "  Allow command?",
            "  ❯ Yes, proceed",
            "    No, and tell codex what to do",
        ]);
        assert_eq!(
            Region::AfterLastPromptMarker.extract(&s),
            vec![
                "  Allow command?",
                "  ❯ Yes, proceed",
                "    No, and tell codex what to do",
            ]
        );
    }
}
