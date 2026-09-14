//! Golden tests for the `remuda-screen` extraction (D-028 §4.2, §13 P0).
//!
//! P0's acceptance is "zero behaviour change". The unit tests inside
//! `promote.rs` and `pty_interaction.rs` already pin each matcher; this file
//! pins the *seam* — that the `&str` adapters in `remuda-driver` and the grid
//! API in `remuda-screen` agree on the same screens, including the recorded
//! shapes that motivated each rule in the first place.
//!
//! Fixture sources: shapes recorded from a live promoted Claude TUI
//! (terminal-promote-1) and the herdr trust-dialog capture in
//! `crates/remuda-testing/fixtures/herdr/session-trust.jsonl`.

use remuda_driver::promote::{ScreenStatus, detect_from_screen, screen_status, strip_ansi};
use remuda_protocol::AgentKind;
use remuda_screen::{Emulator, ScreenGrid};

/// Every screen shape the pre-extraction matchers were written against, with
/// the verdict each one had before the move.
fn corpus() -> Vec<(&'static str, &'static str, Option<ScreenStatus>)> {
    vec![
        (
            "idle composer",
            "\n\u{2500}\u{2500}\u{2500}\n\u{276f} Try \"how do I log an error?\"\n\u{2500}\u{2500}\u{2500}\n",
            Some(ScreenStatus::Idle),
        ),
        (
            "repainted composer, glyph mid-line",
            "\u{1b}[13;1H\u{2500}\u{2500}\u{1b}[14;3H\u{276f} Try \"fix lint errors\"\u{1b}[15;1H\u{2500}\u{2500}",
            Some(ScreenStatus::Idle),
        ),
        (
            "composer behind colour and cursor escapes",
            "\u{1b}[2J\u{1b}[H\u{1b}[1;36mClaude Code\u{1b}[0m v2.1.270\r\n\u{1b}[38;5;240m\u{2500}\u{2500}\u{2500}\u{1b}[0m\r\n\u{1b}[?25h\u{276f} Try \"how do I log an error?\"\r\n",
            Some(ScreenStatus::Idle),
        ),
        (
            "turn in flight",
            "\u{276f} hello\n\u{2726} Thinking… (esc to interrupt)\n",
            Some(ScreenStatus::Working),
        ),
        (
            "trust dialog over a composer",
            "\u{276f} earlier\nQuick safety check:\nIs this a project you created or one you trust?\n\u{276f} Yes, I trust this folder\n",
            Some(ScreenStatus::Blocked),
        ),
        (
            "tool approval",
            "\u{276f} run it\nDo you want to proceed?\n\u{276f} 1. Yes\n2. No\n",
            Some(ScreenStatus::Blocked),
        ),
        ("plain shell", "$ claude\n", None),
        ("empty", "", None),
    ]
}

#[test]
fn the_driver_adapter_and_the_screen_crate_agree_on_every_recorded_shape() {
    for (name, screen, expected) in corpus() {
        assert_eq!(screen_status(screen), expected, "driver adapter: {name}");
        let grid = ScreenGrid::from_raw(&remuda_screen::screen_tail(screen));
        assert_eq!(
            remuda_screen::screen_status(&grid),
            expected,
            "screen crate: {name}"
        );
    }
}

#[test]
fn the_emulator_reaches_the_same_verdict_as_the_stripped_tail() {
    // The point of §4.1: feeding the same bytes through a real terminal must
    // not change what the matchers conclude. Where it *would* differ is a
    // repainting TUI, covered separately below.
    for (name, screen, expected) in corpus() {
        let mut emulator = Emulator::new(100, 30);
        emulator.feed(screen.as_bytes());
        assert_eq!(
            remuda_screen::screen_status(&emulator.grid()),
            expected,
            "emulated: {name}"
        );
    }
}

#[test]
fn a_stale_dialog_scrolled_out_of_view_does_not_keep_the_session_blocked() {
    let stale = format!(
        "Is this a project you created or one you trust?\n{}\n\u{276f} ready\n",
        "filler line\n".repeat(2000)
    );
    assert_eq!(screen_status(&stale), Some(ScreenStatus::Idle));
}

#[test]
fn the_emulator_forgets_a_dialog_the_tui_painted_over_but_the_byte_tail_does_not() {
    // The concrete win §4.1 claims. A TUI answers its dialog and repaints the
    // composer in place; the bytes still hold the dialog, the screen does not.
    // The tail-based matcher is wrong here — it reports Blocked forever — and
    // this test records that difference rather than papering over it.
    let bytes = b"\x1b[2J\x1b[HIs this a project you created or one you trust?\x1b[2J\x1b[H\xe2\x9d\xaf ready for a prompt";
    let mut emulator = Emulator::new(100, 30);
    emulator.feed(bytes);
    assert_eq!(
        remuda_screen::screen_status(&emulator.grid()),
        Some(ScreenStatus::Idle),
        "the emulator sees only the current frame"
    );
    assert_eq!(
        screen_status(&String::from_utf8_lossy(bytes)),
        Some(ScreenStatus::Blocked),
        "the byte tail still carries the overpainted dialog — this is the \
         signature drift D-028 §4.1 sets out to remove"
    );
}

#[test]
fn banner_detection_is_unchanged_through_the_adapter() {
    for (screen, expected) in [
        ("\n ✻ Welcome to Claude Code!\n", Some(AgentKind::Claude)),
        (
            "\u{1b}[1mClaude Code v2.1.270\u{1b}[0m\n",
            Some(AgentKind::Claude),
        ),
        ("$ ls -la\ntotal 12\n", None),
        ("cat /tmp/claude-notes.txt\n", None),
    ] {
        assert_eq!(detect_from_screen(screen), expected, "{screen:?}");
    }
}

#[test]
fn stripping_ansi_before_the_trust_dialog_parser_changes_nothing_on_its_real_input() {
    // The driver adapter routes the trust dialog through `ScreenGrid::from_raw`,
    // which strips ANSI — the pre-extraction parser did not. That is only safe
    // because the sole caller reads the herdr screen with `strip_ansi: true`,
    // so there is nothing left to strip. Pin the equivalence on that input, and
    // record what the extra strip would do on input that still carries escapes:
    // it makes the parser *more* permissive, never less, so it can no more
    // fabricate an auto-trust than it could before.
    let plain = "Quick safety check: Is this a project you created or one you trust?\n                 ❯ No, exit\nYes, I trust this folder";
    assert_eq!(
        remuda_screen::trust_dialog_keys(&ScreenGrid::from_raw(plain)),
        Some(vec!["down".to_owned(), "enter".to_owned()]),
        "already-stripped input — the real caller's shape — must parse as before"
    );
    // Same dialog with colour still on it: it now matches, where the raw-line
    // parser would have failed the `matches!(label, …)` comparison.
    let coloured = "\u{1b}[1mQuick safety check:\u{1b}[0m Is this a project you created or one you trust?\n                    ❯ No, exit\n\u{1b}[32mYes, I trust this folder\u{1b}[0m";
    assert_eq!(
        remuda_screen::trust_dialog_keys(&ScreenGrid::from_raw(coloured)),
        Some(vec!["down".to_owned(), "enter".to_owned()])
    );
    // The D-022 guard that matters is unchanged: anything but this exact
    // dialog, with exactly one cursor, still refuses to answer itself.
    for rejected in [
        "\u{1b}[1mQuick safety check:\u{1b}[0m Is this a project you created or one you trust?\n❯ No, exit\n❯ Yes, I trust this folder",
        "\u{1b}[1mDo you trust this command?\u{1b}[0m\n❯ No, exit\nYes, I trust this folder",
    ] {
        assert!(
            remuda_screen::trust_dialog_keys(&ScreenGrid::from_raw(rejected)).is_none(),
            "{rejected:?}"
        );
    }
}

#[test]
fn the_re_exported_strip_ansi_is_the_same_function() {
    assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m text"), "red text");
    assert_eq!(
        strip_ansi("\u{1b}]0;title\u{7}body"),
        remuda_screen::strip_ansi("\u{1b}]0;title\u{7}body")
    );
}
