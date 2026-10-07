//! D-057 OA6 ma-sdk-state r3: replay the REAL recorded fixtures through the
//! shared driver mapper and fold every mapped observation with the Node's
//! `engine_turn_activity`, proving the Node-side working/idle projection the
//! Hub and phone ultimately consume agrees with the per-root-turn settlement
//! evidence on the exact bytes from the captures.

use remuda_driver::StdoutMapper;
use remuda_node::signal::engine_turn_activity;
use remuda_protocol::{Activity, DriverKind, LifecyclePayload, ObservationPayload};
use remuda_testing::fixtures_dir;
use serde_json::Value;
use std::path::Path;

const SESSION: &str = "66666666-6666-4666-8666-666666666666";

/// Fold one NDJSON fixture the way the live reader + Node do, returning the
/// engine activity implied by each `turn/result` in order. These are recorded
/// stdout replays: no synthetic turn starts are inserted, so workflows attach
/// to the mapper's implicit bucket exactly as a replayed/hydrated stream sees
/// them; the live outstanding-turn FIFO is covered by the mapper unit tests.
fn result_activities(relative: &str) -> Vec<Option<Activity>> {
    let path = fixtures_dir().join(Path::new(relative));
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut mapper = StdoutMapper::new(DriverKind::ClaudeSdk, SESSION);
    let mut out = Vec::new();
    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).expect("fixture line");
        for observation in mapper.map(value).expect("map frame") {
            if let ObservationPayload::Lifecycle(payload) = &observation.body
                && let LifecyclePayload::Native(native) = payload.as_ref()
                && native.native_name == "result"
            {
                out.push(engine_turn_activity(&observation));
            }
        }
    }
    out
}

#[test]
fn ok_session_single_result_idles() {
    assert_eq!(
        result_activities("scripts/ok.jsonl"),
        vec![Some(Activity::Idle)]
    );
}

#[test]
fn twoturn_both_results_idle_their_turns() {
    assert_eq!(
        result_activities("scripts/twoturn.jsonl"),
        vec![Some(Activity::Idle), Some(Activity::Idle)]
    );
}

#[test]
fn stopped_workflow_canary_idles_on_its_single_post_notification_result() {
    // Real capture: task_started, task_updated(killed), task_notification
    // (stopped), then ONE result at process-global index 0. The Node must
    // idle on it — r2 left this root working forever.
    assert_eq!(
        result_activities("claude/claude-workflow-canary-1.jsonl"),
        vec![Some(Activity::Idle)]
    );
}

#[test]
fn two_workflows_idle_only_after_both_terminate() {
    assert_eq!(
        result_activities("scripts/workflow-two.jsonl"),
        vec![None, None, Some(Activity::Idle)],
        "neither intermediate result idles; only the post-second-notification result"
    );
}

#[test]
fn later_turn_workflow_intermediate_with_nonzero_index_keeps_working() {
    assert_eq!(
        result_activities("scripts/workflow-later-turn.jsonl"),
        vec![
            Some(Activity::Idle), // turn 1
            None,                 // turn 2 workflow intermediate (index 1)
            Some(Activity::Idle), // after the stopped notification (index 2)
        ]
    );
}
