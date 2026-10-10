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

/// ma-sdk-state r4 item 5(b): a locally-written root turn opened with the
/// REAL mapper (begin_turn + turn_started_observation, no process) drives
/// engine_turn_activity Working, and its settled result drives Idle —
/// mirroring the Hub replay ownership case.
#[test]
fn engine_activity_follows_a_real_mapper_local_root_turn() {
    let mut mapper = StdoutMapper::new(DriverKind::ClaudeSdk, SESSION);
    mapper.begin_turn();
    let started = mapper.turn_started_observation("msg-local").expect("start");
    assert_eq!(
        engine_turn_activity(&started),
        Some(Activity::Working),
        "the real-mapper local turn start drives working"
    );

    let result = mapper
        .map(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result_index": 0,
            "queued_turn_count": 0,
            "num_turns": 1,
            "session_id": SESSION,
        }))
        .expect("map result");
    let result = result.into_iter().next().expect("one result observation");
    assert_eq!(
        engine_turn_activity(&result),
        Some(Activity::Idle),
        "the settled result for the locally-opened book idles"
    );
}

/// ma-sdk-state r5 item 6 (5b): the Node replay opens a REAL workflow against a
/// REAL per-turn book. Two root turns are opened with the live `begin_turn`
/// (A then B) and a local_workflow task starts against A. While it is open A's
/// result cannot settle; after the workflow terminates A's result STILL does
/// not idle (B is outstanding); only B's final result idles. The workflow frames
/// are the real `system/task_started` / `task_notification` stream frames.
///
/// Deleting `begin_turn` collapses both turns into the implicit replay bucket:
/// once the workflow terminates A's result then sees an empty bucket and idles
/// immediately — an ownership regression this test fails on.
#[test]
fn an_open_workflow_on_turn_a_keeps_results_working_until_turn_b_settles() {
    let system = |subtype: &str, extra: Value| {
        let mut frame = serde_json::json!({
            "type": "system",
            "subtype": subtype,
            "session_id": SESSION,
            "uuid": "11111111-1111-4111-8111-111111111111",
        });
        if let (Some(obj), Some(extra_obj)) = (frame.as_object_mut(), extra.as_object()) {
            for (key, value) in extra_obj {
                obj.insert(key.clone(), value.clone());
            }
        }
        frame
    };
    let result = |index: u64| {
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": format!("step {index}"),
            "stop_reason": "end_turn",
            "num_turns": 1,
            "result_index": index,
            "session_id": SESSION,
            "uuid": "22222222-2222-4222-8222-222222222222",
        })
    };

    let mut mapper = StdoutMapper::new(DriverKind::ClaudeSdk, SESSION);
    // Two locally-written turns outstanding, A in front.
    mapper.begin_turn();
    mapper.begin_turn();

    let map_results = |mapper: &mut StdoutMapper, frame: Value| -> Option<Activity> {
        mapper
            .map(frame)
            .expect("map frame")
            .into_iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
                    LifecyclePayload::Native(native) if native.native_name == "result" => {
                        engine_turn_activity(&obs)
                    }
                    _ => None,
                },
                _ => None,
            })
    };

    // A background workflow opens on the front turn A.
    mapper
        .map(system(
            "task_started",
            serde_json::json!({
                "task_id": "task_5b_wf",
                "tool_use_id": "toolu_5b_wf",
                "task_type": "local_workflow",
                "workflow_name": "5b-wf",
                "description": "turn A workflow",
            }),
        ))
        .expect("map task_started");

    // A's result while the workflow is open: not settled.
    let at_open = map_results(&mut mapper, result(0));
    assert_eq!(
        at_open, None,
        "a result with the workflow open keeps working"
    );

    // The workflow terminates on its own real notification.
    mapper
        .map(system(
            "task_notification",
            serde_json::json!({
                "task_id": "task_5b_wf",
                "tool_use_id": "toolu_5b_wf",
                "status": "completed",
                "summary": "workflow done",
            }),
        ))
        .expect("map task_notification");

    // A's result now, but B is still outstanding: still not the root settle.
    let a_after_workflow = map_results(&mut mapper, result(1));
    assert_eq!(
        a_after_workflow, None,
        "turn A's result cannot idle while turn B is outstanding"
    );

    // B's result settles the last outstanding turn.
    let b_final = map_results(&mut mapper, result(2));
    assert_eq!(
        b_final,
        Some(Activity::Idle),
        "turn B settles the root idle"
    );
}
