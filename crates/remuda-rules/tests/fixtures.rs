//! Verdict fixtures: representative screens per agent kind, hand-written from
//! the pattern strings in the bundled rules, asserted end to end.
//!
//! Each screen is built the way a carrier would build it (D-028 §4.1): the
//! emulator's rendered rows plus whatever OSC payloads the VT retained. The
//! point of each case is named in its assertion — several exist specifically
//! to pin one of the five false-positive anchors from D-028 §10.

use remuda_rules::{Screen, State, bundled};

/// Build an 80-column screen, padding rows the way a real grid does.
fn screen80(lines: &[&str]) -> Screen {
    Screen::new(lines.iter().map(|s| (*s).to_owned()).collect()).with_cols(80)
}

/// Assert the winning rule and the state it sets.
#[track_caller]
fn assert_verdict(kind: &str, screen: &Screen, state: State, rule: &str) {
    let manifest = bundled(kind).expect("bundled manifest");
    let verdict = manifest.evaluate(screen);
    assert_eq!(
        (verdict.state, verdict.rule.as_deref()),
        (state, Some(rule)),
        "{kind}: expected {state}/{rule}, got {:?}/{:?} (evidence {:?})",
        verdict.state,
        verdict.rule,
        verdict.evidence,
    );
    assert!(
        !verdict.evidence.is_empty(),
        "{kind}: a matching rule must quote its evidence"
    );
}

// ---------------------------------------------------------------- claude ---

#[test]
fn claude_idle_prompt_box() {
    let screen = screen80(&[
        "  I've updated the parser and the tests pass.",
        "",
        "╭──────────────────────────────────────────────────────────────────────────╮",
        "│ ❯                                                                        │",
        "╰──────────────────────────────────────────────────────────────────────────╯",
        "  ? for shortcuts                                       Context left: 71%",
    ]);
    assert_verdict("claude", &screen, State::Idle, "live_prompt_box");
    assert!(bundled("claude").expect("m").evaluate(&screen).visible_idle);
}

#[test]
fn claude_live_turn_working() {
    let screen = screen80(&[
        "  Reading the rule table.",
        "",
        "✻ Extracting the manifests… (12s · ↓ 1.2k tokens · esc to interrupt)",
    ]);
    assert_verdict("claude", &screen, State::Working, "live_turn_working");
}

#[test]
fn claude_osc_title_working_outranks_the_screen() {
    // The OSC title carries the spinner at priority 1100, above every screen
    // rule, so a stale idle-looking grid cannot beat a live title.
    let screen = screen80(&[
        "╭──────────────────────────────────────────────────────────────────────────╮",
        "│ ❯                                                                        │",
        "╰──────────────────────────────────────────────────────────────────────────╯",
    ])
    .with_osc_title("⠹ Working on the parser");
    assert_verdict("claude", &screen, State::Working, "osc_title_working");
}

#[test]
fn claude_bash_permission_prompt() {
    // Layout per the rule's own comment in claude.toml, which records the
    // resting shape it was fixed to match (upstream issue #2650):
    //   ❯ 1. Yes / 2. Yes, and don't ask again for: <cmd> / 3. No
    let screen = screen80(&[
        "  I'll run the test suite.",
        "",
        "  Bash command",
        "    cargo test -p remuda-rules --locked",
        "    Run the crate tests",
        "",
        "  Do you want to proceed?",
        "  ❯ 1. Yes",
        "    2. Yes, and don't ask again for: cargo test",
        "    3. No, and tell Claude what to do differently (esc)",
    ]);
    assert_verdict("claude", &screen, State::Blocked, "bash_permission_prompt");
    assert!(
        bundled("claude")
            .expect("m")
            .evaluate(&screen)
            .visible_blocker,
        "an approval dialog is visible blocker evidence"
    );
}

#[test]
fn claude_boxed_permission_prompt_still_blocks_via_the_fallback() {
    // When the dialog is drawn inside a bordered box the `│` gutter defeats
    // the `^\s*❯?\s*1\.` anchors, so the specific rule cannot claim it. The
    // state must still come out blocked — that is what the priority-300
    // legacy_no_prompt_blocker catch-all is for.
    //
    // This is also the case that proves a bordered dialog is not mistaken for
    // the composer: if it were, the priority-950 live_prompt_box rule would
    // win and report *idle* while the agent sits waiting on a human.
    let screen = screen80(&[
        "╭──────────────────────────────────────────────────────────────────────────╮",
        "│ Bash command                                                             │",
        "│   cargo test -p remuda-rules --locked                                    │",
        "│ Do you want to proceed?                                                  │",
        "│ ❯ 1. Yes                                                                 │",
        "│   3. No, and tell Claude what to do differently (esc)                    │",
        "╰──────────────────────────────────────────────────────────────────────────╯",
        "  Tab to amend · Ctrl+E to explain",
    ]);
    let verdict = bundled("claude").expect("m").evaluate(&screen);
    assert_eq!(
        verdict.state,
        State::Blocked,
        "a boxed dialog must not read as idle"
    );
    assert_eq!(verdict.rule.as_deref(), Some("legacy_no_prompt_blocker"));
}

#[test]
fn claude_dialog_box_is_not_mistaken_for_the_composer() {
    // The narrow version of the above: a permission dialog is the only box on
    // screen. `prompt_box_body` must come back empty rather than handing the
    // dialog's rows to the idle rule.
    let screen = screen80(&[
        "╭──────────────────────────────────────────────────────────────────────────╮",
        "│ Do you want to proceed?                                                  │",
        "│ ❯ 1. Yes                                                                 │",
        "╰──────────────────────────────────────────────────────────────────────────╯",
    ]);
    assert!(
        remuda_rules::region_lines("prompt_box_body".parse().expect("region"), &screen).is_empty(),
        "a dialog is not a composer"
    );
    assert_eq!(
        bundled("claude").expect("m").evaluate(&screen).state,
        State::Blocked
    );
}

#[test]
fn claude_transcript_viewer_skips_the_state_update() {
    // Anchor ③: the transcript viewer looks like an approval dialog. It must
    // win on priority (1000) and suppress the update rather than set a state.
    // The rule reads bottom_non_empty_lines(3), which is where claude draws
    // the viewer's footer.
    let screen = screen80(&[
        "     user: extract the rules",
        "     assistant: on it",
        "",
        "  Showing detailed transcript · Ctrl+O to toggle",
        "  ↑↓ scroll · esc to exit",
    ]);
    let verdict = bundled("claude").expect("m").evaluate(&screen);
    assert_eq!(verdict.rule.as_deref(), Some("transcript_viewer"));
    assert_eq!(verdict.state, State::Unknown);
    assert!(
        verdict.skip_state_update,
        "the viewer must suppress the update, not write unknown"
    );
    assert!(
        !(verdict.visible_idle || verdict.visible_blocker || verdict.visible_working),
        "a skipped update is not visible evidence of anything"
    );
}

#[test]
fn claude_model_picker_skips_the_state_update() {
    // Anchor ③ again, the other half: the model picker also mimics a dialog.
    let screen = screen80(&[
        "╭──────────────────────────────────────────────────────────────────────────╮",
        "│ Select model                                                             │",
        "│ ❯ 1. Default (recommended)                                               │",
        "│   2. Opus                                                                │",
        "│                                                                          │",
        "│ Enter to set as default · Esc to cancel                                  │",
        "╰──────────────────────────────────────────────────────────────────────────╯",
    ]);
    let verdict = bundled("claude").expect("m").evaluate(&screen);
    assert_eq!(verdict.rule.as_deref(), Some("model_picker_menu"));
    assert!(verdict.skip_state_update);
}

#[test]
fn claude_empty_screen_is_unknown_not_idle() {
    // The contract that matters most: nothing on screen proves nothing.
    let verdict = bundled("claude")
        .expect("m")
        .evaluate(&screen80(&["", "", ""]));
    assert_eq!(verdict.state, State::Unknown);
    assert!(!verdict.matched(), "no rule should claim a blank screen");
    assert!(verdict.evidence.is_empty());
}

// ----------------------------------------------------------------- codex ---

#[test]
fn codex_working_line() {
    let screen = screen80(&[
        "• Ran cargo check",
        "",
        "• Working (12s • esc to interrupt)",
    ]);
    assert_verdict("codex", &screen, State::Working, "screen_working_fallback");
}

#[test]
fn codex_interrupted_turn_is_not_working() {
    // The `not` gate on screen_working_fallback: an interrupted conversation
    // still shows the working line shape but must not read as working.
    let screen = screen80(&[
        "■ Conversation interrupted",
        "• Working (12s • esc to interrupt)",
    ]);
    let verdict = bundled("codex").expect("m").evaluate(&screen);
    assert_ne!(verdict.rule.as_deref(), Some("screen_working_fallback"));
    assert_ne!(verdict.state, State::Working);
}

#[test]
fn codex_approval_dialog_with_exact_choice_strings() {
    let screen = screen80(&[
        "› Run the migration",
        "",
        "  codex wants to run:",
        "    cargo test --workspace",
        "",
        "  Allow command?",
        "  ❯ Yes, proceed",
        "    No, and tell codex what to do",
    ]);
    assert_verdict("codex", &screen, State::Blocked, "live_strong_blocker");
}

#[test]
fn codex_trust_dialog() {
    // trust_directory anchors on the first line ("\A> You are in …") inside
    // top_non_empty_lines(20), so it only fires on the startup screen.
    let screen = screen80(&[
        "> You are in /work/example-repo",
        "",
        "  Do you trust the contents of this directory?",
        "",
        "  1. Yes, allow codex to work here",
        "  2. No, exit",
    ]);
    assert_verdict("codex", &screen, State::Blocked, "trust_directory");
}

#[test]
fn codex_trust_text_later_in_the_transcript_does_not_fire() {
    // Same words, but not at the top of the screen: the `\A` anchor plus the
    // top_non_empty_lines(20) region keep this from re-triggering mid-session.
    let mut lines = vec!["› earlier prompt", ""];
    lines.extend(std::iter::repeat_n("  transcript filler", 22));
    lines.push("  Do you trust the contents of this directory?");
    let verdict = bundled("codex").expect("m").evaluate(&screen80(&lines));
    assert_ne!(verdict.rule.as_deref(), Some("trust_directory"));
}

#[test]
fn codex_transcript_viewer_skips_the_state_update() {
    let screen = screen80(&[
        "› show me the log",
        "  ↑/↓ to scroll · PgUp/PgDn to page · Home/End to jump · q to quit",
        "  Esc to edit prev",
    ]);
    let verdict = bundled("codex").expect("m").evaluate(&screen);
    assert_eq!(verdict.rule.as_deref(), Some("transcript_viewer"));
    assert!(verdict.skip_state_update);
}

// ------------------------------------------------------------------ grok ---

#[test]
fn grok_stop_chip_is_the_working_anchor() {
    // Anchor ②: grok's startup splash draws its logo in braille, so the
    // working rule anchors on the trailing [stop] chip, not a spinner glyph.
    let screen = screen80(&["⠧ Waiting on subagent… 2.8s   13s ⇣29.7k [stop]"]);
    assert_verdict("grok", &screen, State::Working, "spinner_status_working");
}

#[test]
fn grok_braille_splash_without_a_stop_chip_is_not_working() {
    // The same braille glyphs, no chip: the splash must not read as a turn.
    let screen = screen80(&["⠀⠀⣿⣿⣿⣿⠀⠀", "⠀⣿⣿⣿⣿⣿⣿⠀", "  grok build 0.2.101"]);
    let verdict = bundled("grok").expect("m").evaluate(&screen);
    assert_ne!(verdict.rule.as_deref(), Some("spinner_status_working"));
}

#[test]
fn grok_option_dialog_blocked() {
    let screen = screen80(&[
        "◆ grok wants to run a command",
        "┃  1 (●) Yes, proceed",
        "┃  2 (○) Yes, and allow for this session",
        "┃  3 (○) No, tell grok what to do",
    ]);
    assert_verdict("grok", &screen, State::Blocked, "option_dialog_blocked");
}

#[test]
fn grok_permission_footer_hints_block() {
    let screen = screen80(&[
        "◆ grok wants to edit src/lib.rs",
        "┃  1 (●) Yes",
        "",
        "  1/3:select │ Ctrl+o:yolo │ Ctrl+c:cancel",
    ]);
    // Both option_dialog_blocked (1200) and permission_hints_blocked (1190)
    // match; the higher priority wins and the outcome is the same state.
    let verdict = bundled("grok").expect("m").evaluate(&screen);
    assert_eq!(verdict.state, State::Blocked);
    assert_eq!(verdict.rule.as_deref(), Some("option_dialog_blocked"));
}

#[test]
fn grok_idle_prompt_hints() {
    let screen = screen80(&["  grok is ready", "  Ctrl+.:shortcuts"]);
    assert_verdict("grok", &screen, State::Idle, "prompt_hints_idle");
}

#[test]
fn grok_osc_progress_distinguishes_busy_from_idle() {
    // herdr keeps the payload after `9;`, so these are the exact strings.
    let busy = screen80(&["  grok"]).with_osc_progress("4;1;-1");
    assert_verdict("grok", &busy, State::Working, "osc_progress_working");

    let idle = screen80(&["  grok"]).with_osc_progress("4;0;0");
    assert_verdict("grok", &idle, State::Idle, "osc_progress_idle");
}

#[test]
fn grok_action_required_title_outranks_everything() {
    // Anchor ④'s companion: the blinking title is the top-priority blocker at
    // 1300 when it is present. (Latching across the frames where it blinks out
    // is the caller's job — the engine is per-frame.)
    let screen = screen80(&["⠧ still going… [stop]"]).with_osc_title("⚠ Action Required - grok");
    assert_verdict("grok", &screen, State::Blocked, "osc_title_blocked");
}

// ------------------------------------------------------------------- agy ---

#[test]
fn agy_requesting_permission() {
    let screen = screen80(&[
        "  antigravity is requesting permission for:",
        "    rm -rf ./build",
        "",
        "  Do you want to proceed?",
        "  ❯ Yes",
        "    No",
    ]);
    assert_verdict("agy", &screen, State::Blocked, "permission_prompt");
}

#[test]
fn agy_spinner_working() {
    let screen = screen80(&["⠋ Thinking about the request"]);
    assert_verdict("agy", &screen, State::Working, "spinner_working");
}

#[test]
fn agy_resolves_through_its_aliases() {
    let screen = screen80(&["⠋ Thinking about the request"]);
    for alias in ["agy", "antigravity", "antigravity-cli"] {
        let verdict = bundled(alias).expect("alias resolves").evaluate(&screen);
        assert_eq!(verdict.state, State::Working, "{alias}");
    }
}

// ---------------------------------------------------- anchors ① and ⑤ ------

#[test]
fn anchor_one_user_text_cannot_impersonate_an_activity_line() {
    // Anchor ①: activity lines are anchored at column zero with indented
    // continuations. The same words typed into the composer are indented
    // inside the prompt box, so they must not match live_turn_working.
    let screen = screen80(&[
        "╭──────────────────────────────────────────────────────────────────────────╮",
        "│ ❯ ✻ Extracting the manifests… (12s · esc to interrupt)                   │",
        "╰──────────────────────────────────────────────────────────────────────────╯",
    ]);
    let verdict = bundled("claude").expect("m").evaluate(&screen);
    assert_ne!(
        verdict.rule.as_deref(),
        Some("live_turn_working"),
        "text typed in the composer must not read as an activity line"
    );
}

#[test]
fn anchor_five_soft_wrap_is_joined_before_matching_at_forty_cols() {
    // Anchor ⑤: at 40 columns claude's permission prompt wraps mid-phrase, so
    // `Do you want to proceed?` lands across two grid rows and the rule's
    // `contains` would miss entirely without the rejoin. Rows are padded to
    // the full width the way an emulator hands them back; only the row whose
    // *content* fills all 40 columns is treated as a wrap candidate.
    let wrapped = Screen::new(
        [
            "  Bash command                          ",
            "    cargo test -p remuda-rules --locked ",
            "  Claude wants to run it. Do you want to",
            " proceed?                               ",
            "  ❯ 1. Yes                              ",
            "    3. No, tell Claude what to do       ",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect(),
    )
    .with_cols(40);

    assert!(
        wrapped
            .logical_rows()
            .iter()
            .any(|r| r.to_lowercase().contains("do you want to proceed?")),
        "the wrapped phrase must be rejoined before matching, got {:?}",
        wrapped.logical_rows()
    );
    assert_verdict("claude", &wrapped, State::Blocked, "bash_permission_prompt");
}

#[test]
fn the_same_dialog_reads_the_same_at_eighty_and_forty_cols() {
    // The width must not change the verdict — that is the whole point of
    // normalising soft wrap before matching.
    let at80 = screen80(&[
        "  Do you want to proceed?",
        "  ❯ 1. Yes",
        "    3. No, and tell Claude what to do differently (esc)",
        "  Tab to amend",
    ]);
    let at40 = Screen::new(
        [
            "  Do you want to proceed?              ",
            "  ❯ 1. Yes                             ",
            "    3. No, and tell Claude what to do d",
            "ifferently (esc)                       ",
            "  Tab to amend                         ",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect(),
    )
    .with_cols(39);

    let manifest = bundled("claude").expect("m");
    let a = manifest.evaluate(&at80);
    let b = manifest.evaluate(&at40);
    assert_eq!(a.state, State::Blocked);
    assert_eq!(a.state, b.state, "width must not change the state");
    assert_eq!(a.rule, b.rule, "width must not change the winning rule");
}
