//! §9.1 model-sync node-side integration: both directions and the read-back
//! fold, through the real [`remuda_driver::TranscriptMapper`] over verbatim
//! records captured from a real claude 2.1.272 PTY session
//! (`remuda-driver` fixture `fixtures/model-21272/`).
//!
//! - structured → terminal: an armed bridge settles Applied from the
//!   `<local-command-stdout>` verdict, carrying the *resolved* id the TUI
//!   printed (a typed alias can resolve to a different concrete gateway id);
//! - an unknown id (`system` record) rejects with `not-found` so the UI
//!   reverts;
//! - terminal → structured: a hand-typed `/model` edge is attributed `slash`
//!   and never re-triggers a push-down (no ping-pong).

use std::sync::Arc;
use std::time::{Duration, Instant};

use remuda_driver::TranscriptMapper;
use remuda_driver::test_support::{self, ModelBridgeHandle};
use remuda_protocol::ObservationPayload;

const FOLD_BUDGET_MS: u128 = 50;

const WALK: &str =
    include_str!("../../remuda-driver/tests/fixtures/model-21272/model-walk-21272.jsonl");
const REJECT: &str =
    include_str!("../../remuda-driver/tests/fixtures/model-21272/model-reject-21272.jsonl");

fn line_containing<'a>(haystack: &'a str, needle: &str) -> &'a str {
    haystack
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("fixture line containing {needle:?}"))
}

#[tokio::test]
async fn structured_switch_settles_from_the_stdout_verdict_inside_the_budget() {
    let bridge = Arc::new(ModelBridgeHandle::new());
    let mut mapper =
        test_support::mapper_with_model_bridge(bridge.clone(), "model-sync", "2.1.272");
    let generation = bridge.arm("model_hub/es1_orange_o50");

    let slash = line_containing(WALK, "<command-args>model_hub/es1_orange_o50</command-args>");
    let stdout = line_containing(WALK, "Set model to `model_hub/es1_orange_o50`");

    let started = Instant::now();
    mapper.map_line(slash).expect("slash maps");
    mapper.map_line(stdout).expect("stdout maps");
    let verdict = bridge
        .wait(generation, Duration::from_secs(1))
        .await
        .expect("verdict within 1s");
    assert!(
        started.elapsed().as_millis() <= FOLD_BUDGET_MS,
        "verdict fold exceeded budget"
    );
    match verdict {
        remuda_driver::model::ModelReadback::Applied(observed) => {
            assert_eq!(observed.id, "model_hub/es1_orange_o50");
        }
        other => panic!("expected Applied o50, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_id_rejects_not_found_for_ui_revert() {
    let bridge = Arc::new(ModelBridgeHandle::new());
    let mut mapper =
        test_support::mapper_with_model_bridge(bridge.clone(), "model-sync", "2.1.272");
    let generation = bridge.arm("bogus-xyz-123");
    // Rejects are `system` local_command records (top-level content).
    for line in REJECT.lines().take(2) {
        mapper.map_line(line).expect("system record maps");
    }
    match bridge
        .wait(generation, Duration::from_secs(1))
        .await
        .expect("verdict")
    {
        remuda_driver::model::ModelReadback::Rejected { reason } => {
            assert_eq!(reason, "not-found");
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn terminal_switch_emits_a_slash_attributed_model_edge_without_a_bridge() {
    let mut mapper = unmapped_mapper();
    let mut ids = Vec::new();
    for line in WALK.lines().filter(|l| !l.trim().is_empty()) {
        for obs in mapper.map_line(line).expect("map") {
            if let ObservationPayload::Model(payload) = obs.body {
                ids.push((payload.effective.id, payload.effective.source));
            }
        }
    }
    // The gateway id switch, hand-typed, is attributed to the human.
    assert!(
        ids.iter().any(|(id, source)| {
            id == "model_hub/es1_orange_o50"
                && *source == remuda_protocol::EffortSource::Slash
        }),
        "o50 slash edge missing: {ids:?}"
    );
}

fn unmapped_mapper() -> TranscriptMapper {
    TranscriptMapper::new(
        remuda_protocol::DriverKind::ClaudePty,
        remuda_protocol::InstanceId::new(),
        remuda_protocol::RunId::new(),
        remuda_protocol::Id::new("obj").expect("journal id"),
        remuda_protocol::HostId::new(),
        "model-session".into(),
        "2.1.272".into(),
    )
}
