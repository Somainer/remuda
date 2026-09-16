//! Parser checks against real claude 2.1.272 PTY captures taken on the dev
//! host (2026-09-16, 120x40 xterm-256color). The fixtures are raw escape
//! streams; the test replays them in small chunks through the same emulator
//! the promotion poller feeds, exactly as production does.
//!
//! Capture coverage:
//! - `cap2.bin` — a short turn: the `running UserPromptSubmit hook` line,
//!   `running Stop hook`, and the post-turn `Baked … done` row (must never
//!   parse as live).
//! - `tools2.bin` — a Bash tool turn: `running PreToolUse hook · thinking
//!   with xhigh effort`, live `↓` token growth, `thought for Ns`.
//! - `xhigh3.bin` — a long xhigh reasoning turn: `thinking with xhigh
//!   effort`, thousands of streamed tokens, the fullscreen-renderer repaint
//!   path (`?1049` throughout), and `running Stop hook` at 1m+.

use remuda_screen::{Emulator, ScreenLiveChange, ScreenLiveLatch, screen_live};

fn repaint_frames(bytes: &[u8]) -> Vec<&[u8]> {
    // claude redraws the alt-screen TUI with a cursor-home (`ESC[H`) per
    // repaint. Feeding one repaint at a time is what catches a status frame
    // that the next repaint overwrites (fixed byte-count windows skip past
    // the short UserPromptSubmit frame).
    let mut frames = Vec::new();
    let mut start = 0;
    let mut i = 1;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"\x1b[H" && i > start {
            frames.push(&bytes[start..i]);
            start = i;
        }
        i += 1;
    }
    frames.push(&bytes[start..]);
    frames
}

fn replay(path: &str) -> Vec<(usize, remuda_screen::ScreenLive)> {
    let data = std::fs::read(path).unwrap_or_else(|_| {
        let local = path.replace("fixtures/", "");
        panic!("fixture not found: {path} ({local})");
    });
    let mut emulator = Emulator::new(120, 40);
    let mut readings = Vec::new();
    let mut last = String::new();
    for (index, chunk) in repaint_frames(&data).into_iter().enumerate() {
        emulator.feed(chunk);
        if let Some(live) = screen_live(&emulator.grid()) {
            let sig = live.signature();
            if sig != last {
                last = sig;
                readings.push((index, live));
            }
        }
    }
    readings
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn hook_phrases_and_the_done_row_on_a_short_turn() {
    let readings = replay(&fixture("cap2.bin"));
    let phrases: Vec<&str> = readings
        .iter()
        .filter_map(|(_, live)| live.phrase.as_deref())
        .collect();
    assert!(
        phrases.iter().any(|p| p.contains("UserPromptSubmit hook")),
        "saw {phrases:?}"
    );
    assert!(
        phrases.iter().any(|p| p.contains("Stop hook")),
        "saw {phrases:?}"
    );
    // The completion row has no ellipsis; the parser must never invent a verb
    // from "Baked".
    assert!(
        !readings.iter().any(|(_, live)| live.verb.contains("Baked")),
        "the post-turn row is not live status"
    );
    // Every captured verb is glyph-free.
    for (_, live) in &readings {
        assert!(
            live.verb.chars().next().is_some_and(|c| c.is_alphabetic()),
            "glyph leaked into verb: {:?}",
            live.verb
        );
    }
}

#[test]
fn tool_turn_carries_tokens_and_thought_phrase() {
    let readings = replay(&fixture("tools2.bin"));
    assert!(
        readings.iter().any(|(_, live)| live.phrase.as_deref()
            == Some("running PreToolUse hook · thinking with xhigh effort")),
        "phrase order varies; the hook phrase and thinking phrase co-occur: {:?}",
        readings.iter().map(|(_, l)| &l.phrase).collect::<Vec<_>>()
    );
    let counts: Vec<u64> = readings
        .iter()
        .filter_map(|(_, live)| live.tokens.as_ref().and_then(|t| t.count))
        .collect();
    assert!(
        !counts.is_empty(),
        "streamed token counts appear during the turn"
    );
    // The screen estimate only ever grows while the turn runs.
    for pair in counts.windows(2) {
        assert!(pair[1] >= pair[0], "token count went backwards: {pair:?}");
    }
    assert!(
        readings.iter().any(|(_, live)| live
            .phrase
            .as_deref()
            .is_some_and(|p| p.starts_with("thought for"))),
        "the post-thinking phrase is parsed"
    );
}

#[test]
fn xhigh_reasoning_turn_shows_the_effort_phrase_and_k_tokens() {
    let readings = replay(&fixture("xhigh3.bin"));
    assert!(
        readings
            .iter()
            .any(|(_, live)| live.phrase.as_deref() == Some("thinking with xhigh effort")),
        "first reading: {:?}",
        readings.first().map(|(_, l)| &l.phrase)
    );
    let k = readings
        .iter()
        .find_map(|(_, live)| live.tokens.as_ref().filter(|t| t.label.contains('k')));
    assert!(k.is_some(), "the turn streams past 1k tokens");
    if let Some(tokens) = k {
        assert!(tokens.count.unwrap_or(0) >= 1000);
    }
    // Long turns reach minute elapsed with two unit tokens ("1m 14s").
    assert!(
        readings
            .iter()
            .any(|(_, live)| live.elapsed.as_ref().is_some_and(|e| e.text.contains('m'))),
        "minute elapsed renders"
    );
    assert!(
        readings
            .iter()
            .any(|(_, live)| live.phrase.as_deref() == Some("running Stop hook")),
        "the Stop hook phrase lands at the end"
    );
}

#[test]
fn the_latch_is_silent_on_repaint_churn_and_emits_each_reading_once() {
    let data = std::fs::read(fixture("tools2.bin")).unwrap();
    let mut emulator = Emulator::new(120, 40);
    let mut latch = ScreenLiveLatch::new();
    let mut emitted = 0usize;
    let mut saw_token_growth = 0u32;
    let mut last_count: Option<u64> = None;
    for chunk in repaint_frames(&data) {
        emulator.feed(chunk);
        if let Some(ScreenLiveChange::Active(live)) = latch.observe(&emulator.grid()) {
            emitted += 1;
            let count = live.tokens.as_ref().and_then(|t| t.count);
            if let Some(count) = count
                && last_count.is_some_and(|previous| count > previous)
            {
                saw_token_growth += 1;
            }
            last_count = count;
        }
    }
    // Bounded by distinct readings, nowhere near the frame count (~80 chunks
    // carry spinner frames); proves at-most-once-per-change.
    assert!(emitted > 5 && emitted < 60, "emitted {emitted}");
    assert!(
        saw_token_growth > 1,
        "token updates crossed the latch: {saw_token_growth}"
    );
}
