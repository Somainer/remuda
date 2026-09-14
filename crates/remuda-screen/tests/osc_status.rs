//! The OSC tier, end to end through the real VT parser (design §2.4).
//!
//! Bytes here are byte-exact captures from claude 2.1.270 (`harness-parity.md`
//! probe C, `claude-channels.md` §3.1) rather than synthetic `OscState`s: the
//! whole point of D-3 is that the emulator retains these regions and nothing
//! read them. The D-2 regression drives the actual failure — a 2.1.270 working
//! frame with **zero** occurrences of `interrupt` — and asserts the OSC tier
//! keeps the screen verdict at Working.

use remuda_screen::{
    Emulator, HookHealth, ScreenLatch, ScreenStatus, progress_state, screen_status,
};

/// Feed bytes through a real emulator and classify the rendered grid.
fn classify(bytes: &[u8]) -> Option<ScreenStatus> {
    let mut emulator = Emulator::new(120, 45);
    emulator.feed(bytes);
    screen_status(&emulator.grid())
}

fn poll(bytes: &[u8], latch: &mut ScreenLatch, health: HookHealth) -> Option<ScreenStatus> {
    let mut emulator = Emulator::new(120, 45);
    emulator.feed(bytes);
    latch.update(&emulator.grid(), health)
}

// Golden bytes, transcribed from the captured probes. `3;` with an empty
// percent field is the load-bearing one (harness-parity §2.1).
const OSC_TITLE_IDLE: &[u8] = b"\x1b]0;\xe2\x9c\xb3 Claude Code\x07";
const OSC_TITLE_BUSY_HALF_LEFT: &[u8] = b"\x1b]0;\xe2\x97\x90 HELLO-PROBE bash sleep probe\x07";
const OSC_TITLE_BUSY_HALF_RIGHT: &[u8] = b"\x1b]0;\xe2\x97\x91 HELLO-PROBE bash sleep probe\x07";
const OSC_TITLE_SPARK: &[u8] = b"\x1b]0;\xe2\x9c\xb3 Bash tool probe\x07";
const OSC_PROGRESS_BUSY_EMPTY_PERCENT: &[u8] = b"\x1b]9;4;3;\x07";
const OSC_PROGRESS_BUSY_ZERO_PERCENT: &[u8] = b"\x1b]9;4;3;0\x07";
const OSC_PROGRESS_OFF_EMPTY_PERCENT: &[u8] = b"\x1b]9;4;0;\x07";

/// A 2.1.270-style working screen: random spinner verb + token counter, and —
/// unlike older builds — no `esc to interrupt` footer anywhere.
fn modern_working_frame() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    bytes.extend_from_slice("\u{276f} run a real probe\n".as_bytes());
    bytes.extend_from_slice("\u{23fa} Bash(sleep 20)\n".as_bytes());
    bytes.extend_from_slice("\u{23bf}  Running… (3s)\n".as_bytes());
    bytes.extend_from_slice("\u{273b} Grooving… (14s \u{00b7} \u{2193} 103 tokens)\n".as_bytes());
    bytes.extend_from_slice(b"\x1b[?2026h");
    bytes
}

/// The idle screen 2.1.270 paints at turn end: composer back, spinner line
/// replaced by the done summary.
fn modern_idle_frame() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    bytes.extend_from_slice("\u{273b} Cogitated for 13s \u{00b7} done 12:17 AM\n".as_bytes());
    bytes.extend_from_slice("\u{276f} \n".as_bytes());
    bytes
}

#[test]
fn the_raw_progress_token_is_parsed_from_both_percent_shapes() {
    assert_eq!(progress_state("3"), Some(3));
    assert_eq!(progress_state("3;0"), Some(3));
    assert_eq!(progress_state("0"), Some(0));
}

#[test]
fn title_glyphs_classify_busy_and_idle_off_the_probe_bytes() {
    assert_eq!(
        classify(OSC_TITLE_BUSY_HALF_LEFT),
        Some(ScreenStatus::Working)
    );
    assert_eq!(
        classify(OSC_TITLE_BUSY_HALF_RIGHT),
        Some(ScreenStatus::Working)
    );
    // ✳ alone is ambiguous and the grid has no composer, so it stays unknown
    // rather than collapsing to idle (D-028 §10).
    assert_eq!(classify(OSC_TITLE_IDLE), None);
    assert_eq!(classify(OSC_TITLE_SPARK), None);
    // ✳ plus the ready composer is the idle edge.
    let mut idle = OSC_TITLE_IDLE.to_vec();
    idle.extend_from_slice(&modern_idle_frame());
    assert_eq!(classify(&idle), Some(ScreenStatus::Idle));
}

#[test]
fn progress_sequences_classify_with_the_empty_percent_field() {
    assert_eq!(
        classify(OSC_PROGRESS_BUSY_EMPTY_PERCENT),
        Some(ScreenStatus::Working),
        "9;4;3; — empty percent — is the exact sequence the probe captured"
    );
    assert_eq!(
        classify(OSC_PROGRESS_BUSY_ZERO_PERCENT),
        Some(ScreenStatus::Working)
    );
    assert_eq!(
        classify(OSC_PROGRESS_OFF_EMPTY_PERCENT),
        None,
        "off alone is unknown without a composer frame"
    );
}

#[test]
fn the_spark_with_busy_progress_is_the_permission_dialog() {
    let mut bytes = OSC_TITLE_SPARK.to_vec();
    bytes.extend_from_slice(OSC_PROGRESS_BUSY_EMPTY_PERCENT);
    bytes.extend_from_slice(b"\x1b[2J\x1b[HDo you want to run this command?\n");
    assert_eq!(classify(&bytes), Some(ScreenStatus::Blocked));
}

// ---------------------------------------------------------------------------
// D-2: the live regression
// ---------------------------------------------------------------------------

#[test]
fn d2_a_modern_working_row_without_the_footer_phrase_stays_working_via_osc() {
    let frame = modern_working_frame();
    // Text-only: what `screen_status` saw before this batch. The composer
    // glyph is on screen and the working phrase is gone, so the old rules
    // answer Idle — exactly the reported defect.
    assert_eq!(
        screen_status(&remuda_screen::ScreenGrid::from_raw(
            &remuda_screen::strip_ansi(&String::from_utf8(frame.clone()).unwrap())
        )),
        Some(ScreenStatus::Idle),
        "control: without OSC the modern frame really does misread as idle"
    );

    // …plus what claude actually emits 16–31 ms into the turn.
    let mut with_osc = OSC_TITLE_BUSY_HALF_LEFT.to_vec();
    with_osc.extend_from_slice(OSC_PROGRESS_BUSY_EMPTY_PERCENT);
    with_osc.extend_from_slice(&frame);
    assert_eq!(classify(&with_osc), Some(ScreenStatus::Working));
}

#[test]
fn d2_a_full_modern_turn_never_reports_idle_while_progress_stays_busy() {
    // Repaint the working frame several times, as the real TUI does at ~9 fps.
    let mut stream = Vec::new();
    stream.extend_from_slice(OSC_TITLE_BUSY_HALF_RIGHT);
    stream.extend_from_slice(OSC_PROGRESS_BUSY_EMPTY_PERCENT);
    for _ in 0..5 {
        stream.extend_from_slice(&modern_working_frame());
    }
    assert_eq!(classify(&stream), Some(ScreenStatus::Working));
}

// ---------------------------------------------------------------------------
// Rule 6: raise-only across the turn-end edge
// ---------------------------------------------------------------------------

#[test]
fn nine_four_zero_after_a_busy_edge_is_held_while_hooks_are_healthy() {
    let mut working = OSC_TITLE_BUSY_HALF_LEFT.to_vec();
    working.extend_from_slice(OSC_PROGRESS_BUSY_EMPTY_PERCENT);
    working.extend_from_slice(&modern_working_frame());

    let mut idle = OSC_TITLE_IDLE.to_vec();
    idle.extend_from_slice(OSC_PROGRESS_OFF_EMPTY_PERCENT);
    idle.extend_from_slice(&modern_idle_frame());

    let mut latch = ScreenLatch::new();
    assert_eq!(
        poll(&working, &mut latch, HookHealth::Healthy),
        Some(ScreenStatus::Working)
    );
    assert_eq!(
        poll(&idle, &mut latch, HookHealth::Healthy),
        Some(ScreenStatus::Working),
        "OSC 9;4;0 may not idle the instance while the hook tier is alive; \
         Stop wins the turn-end race and idles on the hook channel"
    );
}

#[test]
fn nine_four_zero_after_a_busy_edge_idles_when_hooks_never_materialised() {
    let mut working = OSC_PROGRESS_BUSY_EMPTY_PERCENT.to_vec();
    working.extend_from_slice(&modern_working_frame());
    let mut idle = OSC_PROGRESS_OFF_EMPTY_PERCENT.to_vec();
    idle.extend_from_slice(&modern_idle_frame());

    let mut latch = ScreenLatch::new();
    assert_eq!(
        poll(&working, &mut latch, HookHealth::NeverMaterialised),
        Some(ScreenStatus::Working)
    );
    assert_eq!(
        poll(&idle, &mut latch, HookHealth::NeverMaterialised),
        Some(ScreenStatus::Idle),
        "with no higher tier the OSC edge is the best authority available"
    );
}

// ---------------------------------------------------------------------------
// Anchor ④: the blocked latch across flicker
// ---------------------------------------------------------------------------

#[test]
fn a_flickered_attention_frame_holds_blocked_until_the_positive_edge() {
    // `⚠ Action Required` drops frames on blur; between two title ✳ frames
    // the grid can show just the composer.
    let mut dialog = OSC_TITLE_SPARK.to_vec();
    dialog.extend_from_slice(OSC_PROGRESS_BUSY_EMPTY_PERCENT);
    dialog.extend_from_slice(b"\x1b[2J\x1b[HBash command\nDo you want to proceed?\n");

    let dropped = b"\x1b[2J\x1b[H\x1b]9;4;3;\x07\x1b]0;\xe2\x9c\xb3 Bash tool probe\x07";

    let mut latch = ScreenLatch::new();
    assert_eq!(
        poll(&dialog, &mut latch, HookHealth::NeverMaterialised),
        Some(ScreenStatus::Blocked)
    );
    // Three polled frames where the dialog text did not paint.
    for _ in 0..3 {
        assert_eq!(
            poll(dropped, &mut latch, HookHealth::NeverMaterialised),
            Some(ScreenStatus::Blocked)
        );
    }
    // The answer lands and the turn resumes: title busy again.
    let mut resumed = OSC_TITLE_BUSY_HALF_LEFT.to_vec();
    resumed.extend_from_slice(OSC_PROGRESS_BUSY_EMPTY_PERCENT);
    assert_eq!(
        poll(&resumed, &mut latch, HookHealth::NeverMaterialised),
        Some(ScreenStatus::Working),
        "a positive working edge must release the blocked latch"
    );
}

// ---------------------------------------------------------------------------
// Full modern-turn fixture, replayed frame by frame
// ---------------------------------------------------------------------------

/// One synchronized repaint boundary: the bytes up to and including one
/// `ESC[2J` frame. The fixture is hand-assembled from measured byte shapes.
fn fixture_frames() -> Vec<Vec<u8>> {
    let bytes = include_bytes!("fixtures/modern-claude-turn.bin").to_vec();
    let mut frames = Vec::new();
    let mut rest = bytes.as_slice();
    let marker = b"\x1b[2J";
    while let Some(idx) = rest
        .windows(marker.len())
        .position(|w| w == marker)
        .map(|i| i + marker.len())
    {
        let (frame, tail) = rest.split_at(idx);
        frames.push(frame.to_vec());
        rest = tail;
    }
    if !rest.is_empty() {
        frames.push(rest.to_vec());
    }
    frames
}

#[test]
fn the_modern_turn_fixture_classifies_idle_working_blocked_working_idle() {
    let frames = fixture_frames();
    assert!(frames.len() >= 5, "fixture frames: {}", frames.len());
    let mut emulator = Emulator::new(120, 45);
    let mut timeline = Vec::new();
    for frame in &frames {
        emulator.feed(frame);
        let status = screen_status(&emulator.grid());
        if timeline.last() != Some(&status) {
            timeline.push(status);
        }
    }
    assert_eq!(
        timeline,
        vec![
            None,
            Some(ScreenStatus::Working),
            Some(ScreenStatus::Blocked),
            Some(ScreenStatus::Working),
            None,
            Some(ScreenStatus::Idle),
        ],
        "the OSC tier must carry the whole turn without `esc to interrupt`; \
         the None frames are mode-set-only and the 9;4;0 edge arriving a \
         repaint before the idle text paints — unknown never collapses to idle"
    );
}

#[test]
fn the_modern_turn_fixture_contains_zero_interrupt_evidence() {
    let bytes = include_bytes!("fixtures/modern-claude-turn.bin");
    assert!(
        !bytes.windows(9).any(|w| w == b"interrupt"),
        "the D-2 regression fixture must not contain the old working phrase"
    );
}
