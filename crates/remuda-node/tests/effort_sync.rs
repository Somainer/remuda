//! §9.1 effort-sync-2 node-side integration: BOTH directions and the
//! read-back latency budget, driven through the real
//! [`remuda_driver::TranscriptMapper`] over verbatim records captured from a
//! real claude 2.1.272 PTY session (`remuda-driver` fixture
//! `fixtures/effort-21272/`).
//!
//! - structured → terminal: an armed bridge settles Applied from the
//!   `<local-command-stdout>` verdict WITHOUT a next-turn assistant record,
//!   and the in-process fold is inside the §9.1 ~2 s budget by orders of
//!   magnitude (network/PTY time is excluded — that budget is measured live
//!   by `examples/effort_probe.rs`, see effort-sync-2.md);
//! - terminal → structured: a hand-typed `/effort` (no pending bridge) emits
//!   an `effort` observation attributed to `slash` — that observation is what
//!   the web store folds into the slider, and the mapper itself never calls
//!   back into anything that could push down again (no ping-pong).
//!
//! Run with `cargo test -p remuda-node --test effort_sync -- --nocapture`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use remuda_driver::TranscriptMapper;
use remuda_driver::test_support::{self, Bridge};
use remuda_protocol::{EffortSource, ObservationPayload};

/// The driver-internal fold budget: from the verdict bytes being mapped to
/// the bridge settling. The end-to-end idle budget is ~2 s including
/// type/confirm/transcript-poll time, measured live (effort-sync-2.md table).
const FOLD_BUDGET_MS: u128 = 50;

const WALK: &str =
    include_str!("../../remuda-driver/tests/fixtures/effort-21272/effort-walk-21272.jsonl");
const REJECT: &str =
    include_str!("../../remuda-driver/tests/fixtures/effort-21272/effort-reject-21272.jsonl");

fn line_containing<'a>(haystack: &'a str, needle: &str) -> &'a str {
    haystack
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("fixture line containing {needle:?}"))
}

#[tokio::test]
async fn structured_to_terminal_settles_from_stdout_inside_the_fold_budget() {
    let bridge = Arc::new(Bridge::new());
    let mut mapper = test_support::mapper_with_bridge(bridge.clone(), "effort-sync", "2.1.272");
    let generation = bridge.arm_ultracode();

    let slash = line_containing(WALK, "<command-args>ultracode</command-args>");
    let stdout = line_containing(WALK, "Set effort level to ultracode");

    let started = Instant::now();
    mapper.map_line(slash).expect("slash maps");
    mapper.map_line(stdout).expect("stdout maps");
    let verdict = bridge
        .wait(generation, Duration::from_secs(1))
        .await
        .expect("verdict within 1s");
    let elapsed = started.elapsed().as_millis();
    assert!(
        elapsed <= FOLD_BUDGET_MS,
        "verdict fold took {elapsed}ms (budget {FOLD_BUDGET_MS}ms)"
    );
    match verdict {
        remuda_driver::effort::Readback::Applied(observed) => {
            assert_eq!(observed.name, remuda_protocol::EffortName::Xhigh);
            assert_eq!(observed.ultracode, Some(true));
        }
        other => panic!("expected Applied ultracode, got {other:?}"),
    }

    // The effort observation the journal carries is emitted at the stdout
    // verdict — the chip can read "ultracode" before any next prompt exists.
    let mut effort = None;
    for obs in mapper
        .map_line(stdout)
        .expect("idempotent-ish remap")
        .into_iter()
    {
        if let ObservationPayload::Effort(payload) = obs.body {
            effort = Some(payload);
        }
    }
    // Note: the second map of the same stdout is deduped (no edge), so the
    // first mapping above already emitted it — assert on the full replay
    // below instead of here.
    assert!(effort.is_none(), "a re-mapped verdict must not double-emit");
}

#[tokio::test]
async fn structured_to_terminal_rejects_carry_the_reason_for_ui_revert() {
    let bridge = Arc::new(Bridge::new());
    let mut mapper = test_support::mapper_with_bridge(bridge.clone(), "effort-sync", "2.1.272");
    let generation = bridge.arm_max();
    // Esc on the max confirmation dialog.
    mapper
        .map_line(line_containing(REJECT, "<command-args>max</command-args>"))
        .expect("slash maps");
    mapper
        .map_line(line_containing(REJECT, "Kept effort level"))
        .expect("kept maps");
    match bridge
        .wait(generation, Duration::from_secs(1))
        .await
        .expect("verdict")
    {
        remuda_driver::effort::Readback::Rejected { reason } => assert_eq!(reason, "dialog-kept"),
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn terminal_to_structured_emits_a_slash_attributed_effort_observation_without_any_bridge() {
    // A fresh, unattached mapper is exactly what a promoted terminal has:
    // nobody armed a switch, the human typed `/effort high` themselves.
    let mut mapper = unmapped_mapper();
    let mut edges = Vec::new();
    for line in WALK.lines().filter(|l| !l.trim().is_empty()) {
        edges.extend(collect_effort(&mut mapper, line));
    }
    // The high switch's stdout edge is attributed to the human (slash) and
    // clears the flag; a later assistant high record corroborates with source
    // unknown (and carries no flag), which is the same tier — the store folds
    // newest-by-observedAt but both are consistent.
    let slash_high = edges
        .iter()
        .find(|p| {
            p.effective.name == remuda_protocol::EffortName::High
                && p.effective.source == EffortSource::Slash
                && p.effective.ultracode == Some(false)
        })
        .expect("high stdout edge attributed to slash with flag cleared");
    assert_eq!(slash_high.effective.ultracode, Some(false));
    // Sanity: ultracode was observed in the same walk.
    assert!(
        edges
            .iter()
            .any(|p| p.effective.name == remuda_protocol::EffortName::Xhigh
                && p.effective.ultracode == Some(true))
    );
}

fn collect_effort(
    mapper: &mut TranscriptMapper,
    line: &str,
) -> Vec<remuda_protocol::EffortPayload> {
    mapper
        .map_line(line)
        .expect("map")
        .into_iter()
        .filter_map(|obs| match obs.body {
            ObservationPayload::Effort(payload) => Some(*payload),
            _ => None,
        })
        .collect()
}

fn unmapped_mapper() -> TranscriptMapper {
    TranscriptMapper::new(
        remuda_protocol::DriverKind::ClaudePty,
        remuda_protocol::InstanceId::new(),
        remuda_protocol::RunId::new(),
        remuda_protocol::Id::new("obj").expect("journal id"),
        remuda_protocol::HostId::new(),
        "effort-session".into(),
        "2.1.272".into(),
    )
}
