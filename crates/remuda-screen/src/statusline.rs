//! Parsing claude's spinner status line off the rendered grid.
//!
//! The line is the TUI's own ephemeral status (design §2.4: the screen tier is
//! status-only, never content). Real captures from claude 2.1.272 on this
//! host vary in field order and punctuation:
//!
//! ```text
//! ✻ Contemplating… (running UserPromptSubmit hook · 0s)
//! ✳ Grooving… (14s · ↓ 103 tokens)
//! · Razzmatazzing… (49m 38s · ↓ 66.0k tokens · thinking some more with xhigh effort)
//! ·Befuddling… (40s · ↓ 200 tokens · thought for 4s)
//! ✳ Thinking… (esc to interrupt)                     # legacy builds
//! ```
//!
//! Fields are therefore recognised by *shape*, never by position:
//!
//! - a token field starts with the `↓` marker (`↓ 66.0k tokens`);
//! - an elapsed field is `49m 38s` / `14s` units only;
//! - anything else inside the parentheses is the spinner phrase;
//! - the interrupt hint may be a parenthesised field *or* its own tip line.
//!
//! The post-turn row (`✻ Baked for 3s · done 9:51 PM`) has no ellipsis and is
//! never mistaken for live status.

use crate::grid::ScreenGrid;

/// How many of the bottom non-empty rows may belong to the status region. The
/// row itself plus a wrapping continuation and/or the tip line.
const STATUS_REGION_ROWS: usize = 8;

/// One parsed spinner status line.
///
/// Every field is optional except the verb: the TUI may omit the token count
/// early in a turn, omit the phrase, or (legacy builds) show only the interrupt
/// hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenLive {
    /// The spinner verb, e.g. `"Razzmatazzing"` (without the trailing `…`).
    pub verb: String,
    /// Elapsed rendered inside the parentheses, when present.
    pub elapsed: Option<ScreenElapsed>,
    /// Streamed token estimate shown after the `↓` marker, when present.
    pub tokens: Option<ScreenTokens>,
    /// The trailing phrase, e.g. `"thinking some more with xhigh effort"` or
    /// `"running UserPromptSubmit hook"`.
    pub phrase: Option<String>,
    /// The screen offers `esc to interrupt` right now.
    pub interruptible: bool,
}

/// The elapsed reading as the TUI printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenElapsed {
    /// Normalised milliseconds (minute = 60 s); `None` if unparasable.
    pub ms: Option<u64>,
    /// The original text, e.g. `"49m 38s"`.
    pub text: String,
}

/// The streamed token estimate as the TUI printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenTokens {
    /// Numeric value after applying the k/m suffix (`66.0k` → 66000). Kept as
    /// the screen computed it: a character estimate, never a usage record.
    pub count: Option<u64>,
    /// Canonical display label, e.g. `"66.0k"` or `"103"`.
    pub label: String,
}

impl ScreenLive {
    /// Stable signature for change detection: emit at most once per change.
    ///
    /// The cycling glyph is excluded; so is the printed elapsed — elapsed is
    /// never transported (design §2.4: the browser renders `now − since` at
    /// 1 Hz), and including it would journal one event per second.
    #[must_use]
    pub fn signature(&self) -> String {
        let mut parts = vec![self.verb.clone()];
        if let Some(tokens) = &self.tokens {
            parts.push(format!("t:{}", tokens.label));
        }
        if let Some(phrase) = &self.phrase {
            parts.push(format!("p:{phrase}"));
        }
        if self.interruptible {
            parts.push("i".into());
        }
        parts.join("|")
    }

    /// Whether the reading carries anything worth journaling.
    ///
    /// The random verb and the elapsed counter are TUI decoration (design
    /// §0.3, §2.4: elapsed is rendered locally): a line that offers only
    /// those must not produce wire traffic — the 20 s interior of a running
    /// tool repaints `Grooving… (14s)` every second and would otherwise leak
    /// one `pty` event per tick. The token estimate, a real phrase, or the
    /// interrupt hint are the screen content the strip exists to show.
    #[must_use]
    pub fn is_informative(&self) -> bool {
        self.tokens.is_some() || self.phrase.is_some() || self.interruptible
    }

    /// Whether the phrase marks the reasoning ("thinking") part of the turn.
    ///
    /// The random spinner verb carries no information (design §0.3); only the
    /// parenthesised phrase says what the harness is actually doing.
    #[must_use]
    pub fn phrase_is_thinking(&self) -> bool {
        self.phrase.as_deref().is_some_and(is_thinking_phrase)
    }
}

fn is_thinking_phrase(phrase: &str) -> bool {
    let lower = phrase.to_ascii_lowercase();
    lower.contains("thinking") || lower.contains("thought")
}

fn split_verb(prefix: &str) -> Option<&str> {
    // The row begins with the cycling spinner glyph (`* · ✢ ✶ ✻ ✽` in real
    // 2.1.272 captures; older builds use ✳/◐/◑), optionally glued without a
    // space, then the verb. Skip one run of non-alphanumeric glyphs; a row
    // without a glyph still parses (the glyph is redrawn ~8 Hz and is
    // frequently mid-repaint when the grid is sampled).
    let trimmed = prefix.trim_start();
    let lead: String = trimmed
        .chars()
        .take_while(|c| !c.is_alphanumeric())
        .collect();
    if lead.is_empty() {
        return Some(trimmed);
    }
    let rest = trimmed[lead.len()..].trim_start();
    let first = rest.chars().next()?;
    first.is_alphabetic().then_some(rest)
}

fn parse_elapsed(field: &str) -> Option<ScreenElapsed> {
    let text = field.trim();
    // `49m 38s`, `1m 5s`, `14s`, `2h 3m` — one or two unit tokens only.
    let mut total_ms = 0u64;
    let mut tokens = 0u32;
    for token in text.split_whitespace() {
        let (digits, unit) =
            token.split_at(token.bytes().take_while(|b| b.is_ascii_digit()).count());
        if digits.is_empty() {
            return None;
        }
        let n: u64 = digits.parse().ok()?;
        let factor = match unit {
            "ms" => 1,
            "s" => 1000,
            "m" => 60_000,
            "h" => 3_600_000,
            _ => return None,
        };
        total_ms = total_ms.saturating_add(n.saturating_mul(factor));
        tokens += 1;
    }
    if tokens == 0 || tokens > 2 {
        return None;
    }
    Some(ScreenElapsed {
        ms: Some(total_ms),
        text: text.to_owned(),
    })
}

fn parse_tokens(field: &str) -> Option<ScreenTokens> {
    let trimmed = field.trim();
    let rest = trimmed.strip_prefix('↓').unwrap_or(trimmed).trim();
    let mut parts = rest.split_whitespace();
    let number = parts.next()?;
    // Require the literal "tokens" noun so a phrase that merely starts with a
    // down arrow can never be read as a count.
    if parts.next() != Some("tokens") {
        return None;
    }
    let (digits, suffix) = number.split_at(
        number
            .bytes()
            .take_while(|b| b.is_ascii_digit() || *b == b'.')
            .count(),
    );
    let value: f64 = digits.parse().ok()?;
    let mult = match suffix {
        "" => 1.0,
        "k" | "K" => 1_000.0,
        "m" | "M" => 1_000_000.0,
        _ => return None,
    };
    if !(0.0..1_000_000_000.0).contains(&value) {
        return None;
    }
    Some(ScreenTokens {
        count: Some((value * mult).round() as u64),
        label: number.to_owned(),
    })
}

fn is_interrupt_hint(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("esc to interrupt") || lower.contains("escape to interrupt")
}

fn parse_tail(verb: &str, tail: &str, interrupt_tip: bool) -> Option<ScreenLive> {
    let mut live = ScreenLive {
        verb: verb.to_owned(),
        elapsed: None,
        tokens: None,
        phrase: None,
        interruptible: interrupt_tip,
    };
    let mut phrases: Vec<String> = Vec::new();
    for field in tail.split('·').map(str::trim).filter(|f| !f.is_empty()) {
        if is_interrupt_hint(field) {
            live.interruptible = true;
        } else if field.starts_with('↓') {
            live.tokens = parse_tokens(field);
            if live.tokens.is_none() {
                // Keep malformed token fields visible as phrase rather than
                // silently dropping screen evidence.
                phrases.push(field.to_owned());
            }
        } else if let Some(elapsed) = parse_elapsed(field) {
            live.elapsed = Some(elapsed);
        } else {
            phrases.push(field.to_owned());
        }
    }
    if !phrases.is_empty() {
        live.phrase = Some(phrases.join(" · "));
    }
    Some(live)
}

/// Find and parse the spinner status line on a rendered grid.
#[must_use]
pub fn screen_live(grid: &ScreenGrid) -> Option<ScreenLive> {
    let rows = grid.bottom_non_empty_lines(STATUS_REGION_ROWS);
    // The interrupt hint can live on its own (tip) line; detect it first over
    // the whole status region so a wrapped hint still counts.
    let interrupt_tip = rows.iter().any(|row| is_interrupt_hint(row));

    for (index, row) in rows.iter().enumerate() {
        let Some(ellipsis) = row.find('…') else {
            continue;
        };
        let Some(head) = split_verb(&row[..ellipsis]) else {
            continue;
        };
        if head.is_empty() || head.len() > 40 || head.contains('(') {
            continue;
        }
        // The verb is whitespace-delimited spinner copy; a wrapped row that
        // swallowed unrelated text is rejected by the parenthesis match below.
        let after = row[ellipsis + '…'.len_utf8()..].trim_start();
        // Narrow terminals (80 cols) wrap the line: the opening parenthesis
        // can sit alone on the next row and the close on a third. Join up to
        // two following rows until both are present, stopping as soon as the
        // tail closes so an unrelated composer row is never swallowed.
        let mut joined = after.to_owned();
        let mut cursor = index + 1;
        for _ in 0..2 {
            let open = joined.find('(');
            if open.is_some_and(|pos| joined[pos..].contains(')')) {
                break;
            }
            let Some(next) = rows.get(cursor) else { break };
            cursor += 1;
            // A hard terminal wrap splits even mid-word ("effor|t"); insert
            // nothing — the boundary carries no space, and real inter-field
            // spaces are already in one of the rows.
            joined.push_str(next);
        }
        let Some(open) = joined.find('(') else {
            continue;
        };
        let Some(close) = joined[open + 1..].find(')') else {
            continue;
        };
        let inner = joined[open + 1..open + 1 + close].trim();
        if inner.is_empty() {
            continue;
        }
        if let Some(live) = parse_tail(head, inner, interrupt_tip) {
            return Some(live);
        }
    }
    None
}

/// What changed between two polls of the status region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenLiveChange {
    /// A spinner status line is on screen; carries the parsed reading.
    ///
    /// `interruptible` is the union of the textual hint (legacy builds) and the
    /// OSC/screen working verdict (2.1.272 retired the hint text; D-2): a
    /// permission dialog is `Blocked`, never interruptible, even while busy.
    Active(ScreenLive),
    /// No live spinner line (or the screen is not working): the strip must
    /// clear the previous reading. Emitted once per leave, like `Active` once
    /// per signature change.
    Inactive,
}

/// Stateful, at-most-once-per-change projection of the status region.
///
/// One instance per PTY, fed on the poller cadence. The cycling spinner glyph
/// never causes an event (it is excluded from [`ScreenLive::signature`]); a new
/// verb/token/phrase reading does.
#[derive(Debug, Default)]
pub struct ScreenLiveLatch {
    /// Last announced signature; `None` before the first announce.
    last: Option<String>,
}

impl ScreenLiveLatch {
    /// Fresh latch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one polled grid.
    pub fn observe(&mut self, grid: &ScreenGrid) -> Option<ScreenLiveChange> {
        self.observe_with(grid, None)
    }

    /// Fold one polled grid with a working verdict from a higher authority.
    ///
    /// Carriers that already know the turn is busy without reading the screen
    /// (the herdr `agent_status` poll) pass `Some(true)`: a torn frame can then
    /// neither clear the reading nor drop the interruptible bit. `Some(false)`
    /// is an authoritative idle and permits the clear.
    pub fn observe_with(
        &mut self,
        grid: &ScreenGrid,
        known_working: Option<bool>,
    ) -> Option<ScreenLiveChange> {
        let working = known_working.unwrap_or_else(|| {
            crate::signature::screen_status(grid) == Some(crate::signature::ScreenStatus::Working)
        });
        // Only an authoritative non-working screen clears the reading: a parse
        // miss on one torn frame mid-turn must not flash the strip empty.
        let Some(mut live) = screen_live(grid) else {
            if !working && self.last.as_deref().is_some_and(|sig| !sig.is_empty()) {
                self.last = Some(String::new());
                return Some(ScreenLiveChange::Inactive);
            }
            return None;
        };
        // Gate on the *parsed* fields before the OSC override stamps
        // interruptible: a decoration-only frame (`Grooving… (14s)`) stays
        // silent however busy the pane is, so a 20 s tool cannot journal one
        // event per spinner repaint.
        if !live.is_informative() {
            return None;
        }
        if working {
            live.interruptible = true;
        }
        let sig = live.signature();
        if self.last.as_deref() == Some(sig.as_str()) {
            return None;
        }
        self.last = Some(sig);
        Some(ScreenLiveChange::Active(live))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(screen: &str) -> Option<ScreenLive> {
        screen_live(&ScreenGrid::from_lines(screen.lines()))
    }

    #[test]
    fn the_owner_screenshot_line_parses_every_field() {
        let live = parse(
            "· Razzmatazzing… (49m 38s · ↓ 66.0k tokens · thinking some more with xhigh effort)",
        )
        .unwrap();
        assert_eq!(live.verb, "Razzmatazzing");
        let elapsed = live.elapsed.clone().unwrap();
        assert_eq!(elapsed.text, "49m 38s");
        assert_eq!(elapsed.ms, Some(49 * 60_000 + 38_000));
        let tokens = live.tokens.clone().unwrap();
        assert_eq!(tokens.label, "66.0k");
        assert_eq!(tokens.count, Some(66_000));
        assert_eq!(
            live.phrase.as_deref(),
            Some("thinking some more with xhigh effort")
        );
        assert!(live.phrase_is_thinking());
        assert!(!live.interruptible);
    }

    #[test]
    fn a_hook_phrase_comes_first_and_the_elapsed_second() {
        // Real 2.1.272 capture: field order varies, so shape — not position —
        // must win.
        let live = parse("✻ Contemplating… (running UserPromptSubmit hook · 0s)").unwrap();
        assert_eq!(live.verb, "Contemplating");
        assert_eq!(live.elapsed.as_ref().unwrap().text, "0s");
        assert_eq!(
            live.phrase.as_deref(),
            Some("running UserPromptSubmit hook")
        );
        assert!(!live.phrase_is_thinking());
        assert!(live.tokens.is_none());
    }

    #[test]
    fn stop_hook_phrase_parses() {
        let live = parse("✻ Nebulizing… (running Stop hook · 3s)").unwrap();
        assert_eq!(live.verb, "Nebulizing");
        assert_eq!(live.elapsed.as_ref().unwrap().ms, Some(3_000));
        assert_eq!(live.phrase.as_deref(), Some("running Stop hook"));
    }

    #[test]
    fn tokens_without_a_k_suffix_are_counts() {
        let live = parse("✳ Grooving… (14s · ↓ 103 tokens)").unwrap();
        assert_eq!(live.verb, "Grooving");
        assert_eq!(live.tokens.as_ref().unwrap().count, Some(103));
    }

    #[test]
    fn a_glyph_glued_to_the_verb_is_stripped() {
        // Historical capture from promotion.rs: no space between glyph/verb.
        let live = parse("·Befuddling… (40s · ↓ 200 tokens · thought for 4s)").unwrap();
        assert_eq!(live.verb, "Befuddling");
        assert_eq!(live.tokens.as_ref().unwrap().count, Some(200));
        assert_eq!(live.phrase.as_deref(), Some("thought for 4s"));
        assert!(live.phrase_is_thinking());
    }

    #[test]
    fn the_legacy_interrupt_phrase_is_detected_inside_and_outside_the_parens() {
        let live = parse("✳ Thinking… (esc to interrupt)").unwrap();
        assert_eq!(live.verb, "Thinking");
        assert!(live.interruptible);
        assert!(live.elapsed.is_none());

        let live = parse("✳ Pondering… (8s)\n   esc to interrupt\n").unwrap();
        assert!(live.interruptible);
        assert_eq!(live.elapsed.as_ref().unwrap().text, "8s");
    }

    #[test]
    fn missing_fields_are_tolerated() {
        let live = parse("✻ Working… (2s)").unwrap();
        assert_eq!(live.verb, "Working");
        assert_eq!(live.elapsed.as_ref().unwrap().ms, Some(2_000));
        assert!(live.tokens.is_none());
        assert!(live.phrase.is_none());

        let live = parse("✻ Working… (↓ 12 tokens)").unwrap();
        assert!(live.elapsed.is_none());
        assert_eq!(live.tokens.as_ref().unwrap().count, Some(12));

        let bare = parse("✻ Working…").is_none();
        assert!(bare, "the parentheses are part of the status shape");
    }

    #[test]
    fn the_post_turn_done_row_is_never_live_status() {
        // Real 2.1.272 capture: completion row, no ellipsis.
        assert!(parse("✻ Baked for 3s · done 9:51 PM").is_none());
    }

    #[test]
    fn only_the_bottom_region_is_considered() {
        let screen = "✻ Working… (1s · ↓ 9 tokens)\n".to_owned() + &"old row\n".repeat(40) + "❯";
        assert!(screen_live(&ScreenGrid::from_lines(screen.lines())).is_none());
    }

    #[test]
    fn a_wrapped_tail_is_joined_with_the_next_row() {
        let live = parse(
            "· Razzmatazzing… (49m 38s · ↓ 66.0k tokens ·\nthinking some more with xhigh effort)",
        )
        .unwrap();
        assert_eq!(
            live.phrase.as_deref(),
            Some("thinking some more with xhigh effort")
        );
    }

    #[test]
    fn an_80_col_wrap_that_pushes_the_open_paren_to_row_two_still_parses() {
        // Real render break measured from the owner's line at 80 columns:
        // row 1 fills exactly before "(", row 2 carries the tail.
        let full =
            "· Razzmatazzing… (49m 38s · ↓ 66.0k tokens · thinking some more with xhigh effort)";
        assert_eq!(full.chars().count(), 82);
        let row1: String = full.chars().take(80).collect();
        let row2: String = full.chars().skip(80).collect();
        assert!(row1.ends_with("effor"), "row1 ends at the wrap: {row1:?}");
        assert!(row2.starts_with('t'));
        let grid =
            ScreenGrid::from_lines(["❯ LIVEPHRASE reason", row1.as_str(), row2.as_str(), "❯"]);
        let live = screen_live(&grid).expect("wrapped line parses across rows");
        assert_eq!(live.verb, "Razzmatazzing");
        assert_eq!(live.tokens.as_ref().unwrap().label, "66.0k");
        assert_eq!(
            live.phrase.as_deref(),
            Some("thinking some more with xhigh effort")
        );
    }

    #[test]
    fn signatures_exclude_the_cycling_glyph_and_the_ticking_elapsed() {
        let a = parse("✻ Working… (2s · ↓ 5 tokens)").unwrap();
        let b = parse("✳ Working… (2s · ↓ 5 tokens)").unwrap();
        assert_eq!(a.signature(), b.signature(), "glyph churn is not a change");
        let ticked = parse("✳ Working… (3s · ↓ 5 tokens)").unwrap();
        assert_eq!(
            a.signature(),
            ticked.signature(),
            "elapsed is rendered locally, never transported"
        );
        let counted = parse("✳ Working… (3s · ↓ 6 tokens)").unwrap();
        assert_ne!(a.signature(), counted.signature(), "a token change is");
    }

    #[test]
    fn malformed_numbers_do_not_crash_the_parser() {
        assert!(parse_tokens("↓ tokens").is_none());
        assert!(parse_tokens("↓ x tokens").is_none());
        assert!(parse_tokens("↓ 99z tokens").is_none());
        assert!(parse_elapsed("").is_none());
        assert!(parse_elapsed("7x").is_none());
    }

    #[test]
    fn latch_emits_once_per_change_and_clears_when_the_turn_ends() {
        let mut latch = ScreenLiveLatch::new();
        let frame = ScreenGrid::from_lines(["✻ Working… (2s · ↓ 5 tokens)"]);
        // No OSC: a line alone is not authoritative Working, but the reading
        // still emits (legacy builds prove working via the line itself).
        let first = latch.observe(&frame).expect("first frame emits");
        match first {
            ScreenLiveChange::Active(live) => assert_eq!(live.verb, "Working"),
            ScreenLiveChange::Inactive => panic!("active frame"),
        }
        assert!(latch.observe(&frame).is_none(), "identical frame is silent");
        let next = ScreenGrid::from_lines(["✳ Working… (3s · ↓ 6 tokens)"]);
        assert!(matches!(
            latch.observe(&next),
            Some(ScreenLiveChange::Active(_))
        ));
        let idle = ScreenGrid::from_lines(["❯ "]);
        assert!(matches!(
            latch.observe(&idle),
            Some(ScreenLiveChange::Inactive)
        ));
        assert!(latch.observe(&idle).is_none(), "idle stays silent");
    }

    #[test]
    fn latch_does_not_clear_on_a_parse_miss_while_working() {
        let mut latch = ScreenLiveLatch::new();
        let mut working = ScreenGrid::from_lines(["✳ Working… (2s · ↓ 5 tokens)"]);
        working.osc.title = Some("\u{25d0} probe".into());
        working.osc.progress = Some("3".into());
        assert!(matches!(
            latch.observe(&working),
            Some(ScreenLiveChange::Active(live)) if live.interruptible
        ));
        // A dropped/torn frame with no parseable line but OSC still busy.
        let torn = ScreenGrid::from_lines(["❯ "]);
        let mut torn = torn;
        torn.osc.title = Some("\u{25d0} probe".into());
        torn.osc.progress = Some("3".into());
        assert!(
            latch.observe(&torn).is_none(),
            "a torn busy frame keeps the reading"
        );
    }

    #[test]
    fn an_elapsed_only_spinner_frame_stays_silent_through_a_long_tool() {
        // The fake 2.1.270 Running phase paints `✻ Grooving… (Ns)` once a
        // second for the whole 20 s tool: verb constant, only elapsed moves.
        // Nothing here is worth a wire event; the OSC busy edge must not
        // change that.
        let mut latch = ScreenLiveLatch::new();
        for seconds in 1..20 {
            let mut grid = ScreenGrid::from_lines([format!("✻ Grooving… ({seconds}s)").as_str()]);
            grid.osc.title = Some("\u{25d0} probe".into());
            grid.osc.progress = Some("3".into());
            assert!(
                latch.observe(&grid).is_none(),
                "second {seconds}: an elapsed-only frame emitted"
            );
        }
        // A frame that finally carries real content still emits.
        let mut rich =
            ScreenGrid::from_lines(["✻ Grooving… (20s · ↓ 120 tokens · thought for 1s)"]);
        rich.osc.title = Some("\u{25d0} probe".into());
        rich.osc.progress = Some("3".into());
        assert!(matches!(
            latch.observe(&rich),
            Some(ScreenLiveChange::Active(_))
        ));
    }

    #[test]
    fn captured_glyph_variants_all_strip_to_the_verb() {
        for row in [
            "* Forging… (3s)",
            "· Forging… (3s)",
            "✢ Forging… (3s)",
            "✶ Forging… (3s)",
            "✻ Forging… (3s)",
            "✽ Forging… (3s · ↓ 138 tokens · thought for 9s)",
            "Forging… (3s)",
        ] {
            assert_eq!(parse(row).unwrap().verb, "Forging", "{row}");
        }
    }
}
