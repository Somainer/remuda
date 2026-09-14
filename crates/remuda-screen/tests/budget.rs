//! Memory and CPU budget for N emulators (D-028 §13 P0, risk #4).
//!
//! The risk register lists "emulator memory/CPU unmeasured (N × maxInstances)"
//! as a P0 blocker and requires the phase to "come with a benchmark and bound
//! the scrollback lines". This file is that guard.
//!
//! ## The shape of the cost
//!
//! vt100 allocates every scrollback row eagerly at the terminal's current
//! width — `Row::new(cols)` fills a `Vec<Cell>` and `Cell` is a fixed 32 bytes
//! — so a full emulator costs about `(rows + scrollback) × cols × 32 B`
//! regardless of what the rows contain. That makes the bound predictable and
//! makes *columns*, not content, the variable that decides whether the fleet
//! fits.
//!
//! ## The budget
//!
//! `maxInstances` defaults to 8 and is configurable; the design's stated
//! ceiling is 32. At `DEFAULT_SCROLLBACK_LINES` = 1000 and the widest terminal
//! the emulator accepts (`MAX_COLS` = 400), one emulator is ~15 MiB and 32 of
//! them ~494 MiB — far over budget at that extreme, which is why `MAX_COLS`
//! exists as a second bound rather than the only one. At a realistic 200
//! columns it is ~7.7 MiB each and ~247 MiB for 32: inside the 256 MiB
//! asserted below, but only just. The cap is sized so that a full fleet of
//! wide terminals fits, and it has no room to grow without lowering
//! `MAX_COLS` or raising the budget deliberately.
//!
//! Measured on the P0 host (arm64 macOS); see
//! `docs/design/evidence/native-pty-0.md` for the measured table and for why
//! the 2000-line cap the design offered as an example was rejected.
//!
//! The assertions are deliberately loose — real headroom over the measured
//! figures — because they run on whatever CI provides. They exist to catch a
//! *regression in kind* (an emulator that starts retaining output, a cap that
//! silently grows), not to pin an allocator.

use remuda_screen::{DEFAULT_SCROLLBACK_LINES, Emulator, MAX_COLS};
use std::time::Instant;

/// Design ceiling for concurrent instances on one Node.
const MAX_INSTANCES: usize = 32;

/// Per-emulator ceiling at a realistic width, in bytes.
const BUDGET_PER_EMULATOR: usize = 8 * 1024 * 1024;

/// Fleet ceiling at a realistic width, in bytes.
const BUDGET_FLEET: usize = 256 * 1024 * 1024;

/// A realistic wide terminal: wider than a default 80-column window and wider
/// than most maximised ones, but not the 400-column extreme `MAX_COLS` allows.
const REALISTIC_COLS: u16 = 200;
const REALISTIC_ROWS: u16 = 50;

/// Allocator and bookkeeping overhead above the raw cell arithmetic.
///
/// Measured against real RSS across six (columns × scrollback) combinations at
/// 32 emulators: the ratio of measured to computed ran 1.01–1.13, rising with
/// width. 1.15 is the round number above all of them. Baking it in keeps these
/// assertions deterministic — they are computed from constants, not sampled —
/// while still describing what the process actually allocates.
const OVERHEAD_NUM: usize = 115;
const OVERHEAD_DEN: usize = 100;

/// Size of one full emulator, including [`OVERHEAD_NUM`] overhead.
///
/// vt100 pre-allocates every row, so the cell arithmetic is the steady-state
/// heap for the grids rather than a high-water mark.
fn grid_bytes(cols: u16, rows: u16, scrollback: usize) -> usize {
    const CELL: usize = 32;
    // Primary grid rows plus its scrollback, and the alternate grid, which
    // vt100 constructs with a scrollback length of zero.
    let primary = (usize::from(rows) + scrollback) * usize::from(cols) * CELL;
    let alternate = usize::from(rows) * usize::from(cols) * CELL;
    (primary + alternate) * OVERHEAD_NUM / OVERHEAD_DEN
}

/// Fill an emulator's scrollback and viewport completely.
fn fill(emulator: &mut Emulator, scrollback: usize, cols: u16) {
    let body = "x".repeat(usize::from(cols) / 2);
    for i in 0..(scrollback + 100) {
        emulator.feed(format!("\x1b[32m{i:06}\x1b[0m {body}\r\n").as_bytes());
    }
}

#[test]
fn the_default_cap_keeps_one_emulator_inside_its_budget() {
    let bytes = grid_bytes(REALISTIC_COLS, REALISTIC_ROWS, DEFAULT_SCROLLBACK_LINES);
    assert!(
        bytes <= BUDGET_PER_EMULATOR,
        "one emulator at {REALISTIC_COLS}x{REALISTIC_ROWS} with \
         {DEFAULT_SCROLLBACK_LINES} scrollback lines is {} MiB, over the {} \
         MiB per-instance budget",
        bytes / 1024 / 1024,
        BUDGET_PER_EMULATOR / 1024 / 1024
    );
}

#[test]
fn the_default_cap_keeps_a_full_fleet_inside_its_budget() {
    let fleet =
        grid_bytes(REALISTIC_COLS, REALISTIC_ROWS, DEFAULT_SCROLLBACK_LINES) * MAX_INSTANCES;
    assert!(
        fleet <= BUDGET_FLEET,
        "{MAX_INSTANCES} emulators at {REALISTIC_COLS} columns and \
         {DEFAULT_SCROLLBACK_LINES} scrollback lines is {} MiB, over the {} \
         MiB fleet budget — lower DEFAULT_SCROLLBACK_LINES or MAX_COLS",
        fleet / 1024 / 1024,
        BUDGET_FLEET / 1024 / 1024
    );
}

#[test]
fn doubling_the_cap_would_breach_the_fleet_budget() {
    // Records *why* the cap is 1000 rather than the 2000 the design offered as
    // an example: this fails the day someone raises it without also lowering
    // MAX_COLS or deliberately raising the budget.
    let doubled = grid_bytes(REALISTIC_COLS, REALISTIC_ROWS, 2000) * MAX_INSTANCES;
    assert!(
        doubled > BUDGET_FLEET,
        "2000 lines now fits the fleet budget ({} MiB); revisit \
         DEFAULT_SCROLLBACK_LINES, this test, and the evidence doc together",
        doubled / 1024 / 1024
    );
}

#[test]
fn a_full_emulator_allocates_what_the_model_predicts() {
    // The analytic bound is only trustworthy while vt100 keeps pre-allocating
    // rows and evicting past the cap. If it ever started *retaining* output the
    // model becomes an underestimate and both budgets above go with it.
    let scrollback = 200;
    let mut emulator = Emulator::with_scrollback(REALISTIC_COLS, 20, scrollback);
    fill(&mut emulator, scrollback, REALISTIC_COLS);
    assert_eq!(emulator.scrollback_lines(), scrollback);
    assert_eq!(
        emulator.grid().lines.len(),
        20,
        "the visible grid stays the viewport however much has scrolled past"
    );
    // A repaint is the viewport, not the scrollback: it must not grow with how
    // long the session has run. Generous bound — colour runs and cursor motion
    // make the exact size renderer-dependent.
    let repaint = emulator.repaint().len();
    let viewport = usize::from(REALISTIC_COLS) * 20;
    assert!(
        repaint < viewport * 8,
        "repaint is {repaint} bytes for a {viewport}-cell viewport; a snapshot \
         that scaled with scrollback would defeat §4.6"
    );
}

#[test]
fn feeding_a_full_scrollback_stays_well_under_a_second() {
    // CPU side of risk #4. The emulator sits on the PTY read path, so per-byte
    // cost matters; this bounds the worst realistic burst — a whole scrollback
    // arriving at once, as when someone `cat`s a large file.
    let scrollback = DEFAULT_SCROLLBACK_LINES;
    let mut emulator = Emulator::with_scrollback(REALISTIC_COLS, REALISTIC_ROWS, scrollback);
    let started = Instant::now();
    fill(&mut emulator, scrollback, REALISTIC_COLS);
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_millis() < 1000,
        "parsing a full {scrollback}-line scrollback took {elapsed:?}; at that \
         cost the emulator would be visible on the PTY read path"
    );
}

#[test]
fn the_width_clamp_bounds_the_worst_case_a_client_can_ask_for() {
    // MAX_COLS is the second bound: without it one client asking for a
    // 10000-column terminal would blow the fleet budget by itself.
    let mut emulator = Emulator::new(u16::MAX, 50);
    assert_eq!(emulator.size().0, MAX_COLS);
    emulator.resize(u16::MAX, u16::MAX);
    assert_eq!(emulator.size().0, MAX_COLS);
    let worst = grid_bytes(MAX_COLS, REALISTIC_ROWS, DEFAULT_SCROLLBACK_LINES);
    assert!(
        worst <= 16 * 1024 * 1024,
        "one maximally wide emulator is {} MiB; that is the ceiling a single \
         unusual client can reach",
        worst / 1024 / 1024
    );
}

#[test]
fn the_absolute_worst_case_fleet_is_over_budget_and_that_is_recorded_not_hidden() {
    // Honesty about the bound's shape. The 256 MiB fleet budget holds at
    // realistic widths; a full fleet of 32 *maximally wide* terminals does not
    // fit it, and no line-count cap can make it fit without crippling ordinary
    // sessions — the cost is per cell, and a line cap does not know how wide a
    // line is.
    //
    // Left as-is for P0 on the judgement that 32 concurrent 400-column
    // sessions is not a configuration anyone runs: `maxInstances` defaults to
    // 8 (~124 MiB even at that width), and a terminal wider than ~250 columns
    // means a deliberately stretched window. The fix, if this ever bites, is a
    // byte budget rather than a line cap — scrollback derived from width so
    // the product is constant. That is a bigger change than P0 warrants and is
    // noted in docs/design/evidence/native-pty-0.md rather than guessed at
    // here.
    let worst_fleet =
        grid_bytes(MAX_COLS, REALISTIC_ROWS, DEFAULT_SCROLLBACK_LINES) * MAX_INSTANCES;
    assert!(
        worst_fleet > BUDGET_FLEET,
        "the worst case now fits ({} MiB) — the caveat in the evidence doc and \
         this test are stale, delete both",
        worst_fleet / 1024 / 1024
    );
    // The default fleet size must fit even at maximum width: that is the case
    // an ordinary user can actually reach by opening wide windows.
    let default_fleet = grid_bytes(MAX_COLS, REALISTIC_ROWS, DEFAULT_SCROLLBACK_LINES) * 8;
    assert!(
        default_fleet <= BUDGET_FLEET,
        "the default 8 instances at maximum width is {} MiB, over the {} MiB \
         fleet budget — this one is reachable in normal use and must fit",
        default_fleet / 1024 / 1024,
        BUDGET_FLEET / 1024 / 1024
    );
}
