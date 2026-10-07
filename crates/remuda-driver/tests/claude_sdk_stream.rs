//! Stream-assembly units for the `claude-sdk` carrier over recorded NDJSON.
//!
//! These replay fixtures through the same `map_outbound` the driver's reader
//! task calls, so an assertion here is an assertion about production behaviour.
//! The properties under test are `print-replacement.md` §1.7 and §2.5 plus
//! D-028a item 3: one message node across every delta, `Partial` completeness
//! while a block is open and `Structured` once the final block closes it,
//! thought / tool_call / tool_result ordering, and usage from the terminal
//! `result`.

use remuda_driver::StdoutMapper;
use remuda_protocol::{
    Completeness, ContentBlock, ContentStatus, DriverKind, Knowledge, LifecyclePayload,
    MutationOperation, Observation, ObservationPayload, SourceChannel, U64,
};
use serde_json::Value;

const SESSION: &str = "66666666-6666-4666-8666-666666666666";

fn replay(source: &str, driver: DriverKind) -> (StdoutMapper, Vec<Observation>) {
    let mut mapper = StdoutMapper::new(driver, "");
    let mut out = Vec::new();
    for line in source.lines().filter(|l| !l.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).expect("fixture line");
        out.extend(mapper.map(value).expect("map frame"));
    }
    (mapper, out)
}

fn partial_then_final() -> (StdoutMapper, Vec<Observation>) {
    replay(
        include_str!("fixtures/claude-sdk-partial-then-final.jsonl"),
        DriverKind::ClaudeSdk,
    )
}

fn kind_name(obs: &Observation) -> &'static str {
    match &obs.body {
        ObservationPayload::Message(_) => "message",
        ObservationPayload::Thought(_) => "thought",
        ObservationPayload::ToolCall(_) => "tool_call",
        ObservationPayload::ToolResult(_) => "tool_result",
        ObservationPayload::Usage(_) => "usage",
        ObservationPayload::Lifecycle(_) => "lifecycle",
        ObservationPayload::Opaque(_) => "opaque",
        _ => "other",
    }
}

fn text_of(obs: &Observation) -> String {
    match &obs.body {
        ObservationPayload::Message(payload) => payload
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

fn first_index(obs: &[Observation], name: &str) -> usize {
    obs.iter()
        .position(|o| kind_name(o) == name)
        .unwrap_or_else(|| {
            panic!(
                "no {name} in {:?}",
                obs.iter().map(kind_name).collect::<Vec<_>>()
            )
        })
}

/// Every delta of one native message folds onto **one** node, and the final
/// assistant block is authoritative (§1.7, D-028a item 3).
#[test]
fn stream_deltas_share_one_message_node_id() {
    let (_mapper, obs) = partial_then_final();
    let messages: Vec<&Observation> = obs.iter().filter(|o| kind_name(o) == "message").collect();
    assert!(
        messages.len() >= 4,
        "expected three deltas plus a final block, got {}",
        messages.len()
    );

    let node_ids: Vec<_> = messages
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::Message(p) => Some(p.mutation.node_id.clone()),
            _ => None,
        })
        .collect();
    assert!(
        node_ids.windows(2).all(|w| w[0] == w[1]),
        "stream deltas drew more than one card: {node_ids:?}"
    );

    // The id is the message id, and the first mutation opens it.
    if let ObservationPayload::Message(first) = &messages[0].body {
        assert_eq!(first.message_id, first.mutation.node_id);
        assert_eq!(first.mutation.operation, MutationOperation::Open);
        assert_eq!(
            first.native_origin,
            Knowledge::Known {
                value: "msg_recorded_sdk_1".into()
            }
        );
    }

    // Deltas concatenate to the text the final block carries.
    let streamed: String = messages
        .iter()
        .filter(|o| {
            matches!(&o.body, ObservationPayload::Message(p) if p.status == ContentStatus::Streaming)
        })
        .map(|o| text_of(o))
        .collect();
    assert_eq!(streamed, "Reading the notes file now.");
    let final_text = text_of(messages.last().expect("final message"));
    assert_eq!(final_text, "Reading the notes file now.");
}

/// §2.5: a stream delta is `Partial`; the closing block is `Structured`. Control
/// frames are always `Structured`.
#[test]
fn deltas_are_partial_and_the_final_block_is_structured() {
    let (_mapper, obs) = partial_then_final();

    for o in obs.iter().filter(|o| kind_name(o) == "message") {
        let ObservationPayload::Message(payload) = &o.body else {
            unreachable!()
        };
        match payload.status {
            ContentStatus::Streaming => assert_eq!(
                o.completeness,
                Completeness::Partial,
                "an open block must be Partial"
            ),
            _ => assert_eq!(
                o.completeness,
                Completeness::Structured,
                "a closed block must be Structured"
            ),
        }
    }
    // At least one of each, or the assertion above is vacuous.
    let partials = obs
        .iter()
        .filter(|o| o.completeness == Completeness::Partial)
        .count();
    assert!(partials >= 3, "expected the text deltas to be Partial");
    assert!(
        obs.iter()
            .any(|o| kind_name(o) == "message" && o.completeness == Completeness::Structured),
        "expected a Structured final block"
    );

    // Lifecycle, usage and tool_result are control frames, never Partial.
    for o in obs
        .iter()
        .filter(|o| matches!(kind_name(o), "lifecycle" | "usage" | "tool_result"))
    {
        assert_eq!(
            o.completeness,
            Completeness::Structured,
            "{} must be Structured",
            kind_name(o)
        );
    }
}

/// `claude-print` is unchanged by the parameter: it keeps every content
/// observation `Structured`, which is what its consumers were built against.
#[test]
fn print_keeps_structured_completeness_on_the_same_frames() {
    let (_mapper, obs) = replay(
        include_str!("fixtures/claude-sdk-partial-then-final.jsonl"),
        DriverKind::ClaudePrint,
    );
    assert!(
        obs.iter().all(|o| o.completeness != Completeness::Partial),
        "print must not start reporting Partial"
    );
    for o in &obs {
        assert_eq!(o.source.driver_kind, DriverKind::ClaudePrint);
    }
}

/// §2.5 ordering: the thought precedes the message, the tool call precedes its
/// result, and the result is its own node joined by `tool_call_id`.
#[test]
fn thought_tool_call_and_tool_result_arrive_in_order() {
    let (_mapper, obs) = partial_then_final();
    let thought = first_index(&obs, "thought");
    let message = first_index(&obs, "message");
    let call = first_index(&obs, "tool_call");
    let result = first_index(&obs, "tool_result");
    assert!(
        thought < message && message < call && call < result,
        "order was thought={thought} message={message} call={call} result={result}"
    );

    // The tool result is its own node, joined to the call rather than mutating
    // it (`NativeIds::tool_result_*`).
    let call_id = obs
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::ToolCall(p) => Some(p.tool_call_id.clone()),
            _ => None,
        })
        .expect("tool call id");
    let (result_node, joined) = obs
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::ToolResult(p) => {
                Some((p.mutation.node_id.clone(), p.tool_call_id.clone()))
            }
            _ => None,
        })
        .expect("tool result");
    assert_eq!(joined, call_id, "result must join the call by tool_call_id");
    assert_ne!(result_node, call_id, "result must be its own node");
}

/// The session id comes from `system/init` (§2.2 item 1) and is stamped on
/// every observation, which is what the Node lifts into `nativeRef`.
#[test]
fn session_identity_comes_from_system_init() {
    let (mapper, obs) = partial_then_final();
    assert_eq!(mapper.session_id(), SESSION);

    let started = obs
        .iter()
        .find(|o| match &o.body {
            ObservationPayload::Lifecycle(p) => match p.as_ref() {
                LifecyclePayload::Native(n) => n.native_name == "session",
                _ => false,
            },
            _ => false,
        })
        .expect("session lifecycle");
    let ObservationPayload::Lifecycle(payload) = &started.body else {
        unreachable!()
    };
    let LifecyclePayload::Native(native) = payload.as_ref() else {
        unreachable!()
    };
    assert_eq!(
        native.native_id,
        Knowledge::Known {
            value: SESSION.into()
        }
    );
    assert_eq!(
        native.status,
        Knowledge::Known {
            value: "started".into()
        }
    );

    for o in &obs {
        assert_eq!(o.source.driver_kind, DriverKind::ClaudeSdk);
        assert_eq!(o.source.channel, SourceChannel::Stdout);
    }
}

/// Usage rides the terminal `result` (§2.5, `usage_from_result`); the usage page
/// regresses if this stops arriving on the new carrier (§2.8).
#[test]
fn usage_comes_from_the_terminal_result() {
    let (_mapper, obs) = partial_then_final();
    let usage = obs
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::Usage(p) => Some(p.as_ref()),
            _ => None,
        })
        .expect("usage observation");
    assert_eq!(usage.input_tokens, Knowledge::Known { value: U64(11) });
    assert_eq!(usage.output_tokens, Knowledge::Known { value: U64(7) });
    assert_eq!(usage.total_tokens, Knowledge::Known { value: U64(18) });
    match &usage.cost {
        Knowledge::Known { value } => assert_eq!(value.currency, "USD"),
        other => panic!("expected a reported cost, got {other:?}"),
    }

    // The turn lifecycle precedes its usage, and a terminal result affects
    // completion (`map_result`).
    let turn_done = obs
        .iter()
        .position(|o| match &o.body {
            ObservationPayload::Lifecycle(p) => match p.as_ref() {
                LifecyclePayload::Native(n) => {
                    n.status
                        == Knowledge::Known {
                            value: "turn_done".into(),
                        }
                        && n.affects_completion
                }
                _ => false,
            },
            _ => false,
        })
        .expect("terminal turn_done");
    let usage_at = first_index(&obs, "usage");
    assert!(turn_done < usage_at, "usage must follow its result");
}

/// The recorded print fixture still assembles on the sdk carrier: the two
/// carriers share one assembler, so one stream shape cannot regress the other.
#[test]
fn the_recorded_print_stream_still_assembles_on_sdk() {
    let (_mapper, obs) = replay(
        include_str!("fixtures/claude-print-stream.jsonl"),
        DriverKind::ClaudeSdk,
    );
    let messages: Vec<&Observation> = obs.iter().filter(|o| kind_name(o) == "message").collect();
    assert_eq!(messages.len(), 12, "ten deltas, one snapshot, one close");
    let ids: Vec<_> = messages
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::Message(p) => Some(p.mutation.node_id.clone()),
            _ => None,
        })
        .collect();
    assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
    assert!(
        obs.iter().any(|o| o.completeness == Completeness::Partial),
        "sdk must report Partial on this fixture's deltas"
    );
}

/// Collect the native `turn/result` lifecycles emitted by replaying `source`.
fn result_natives(source: &str, driver: DriverKind) -> Vec<remuda_protocol::NativeLifecycle> {
    let (_mapper, obs) = replay(source, driver);
    obs.into_iter()
        .filter_map(|o| match o.body {
            ObservationPayload::Lifecycle(p) => match *p {
                LifecyclePayload::Native(n) if n.native_name == "result" => Some(*n),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn settles_root_turn(n: &remuda_protocol::NativeLifecycle) -> bool {
    n.related_ids.get("settledRootTurn").map(String::as_str) == Some("true")
}

/// D-057 OA6 r2 item 1: the driver stamps `settledRootTurn` on the result that
/// ends the ROOT turn, independent of `affectsCompletion` (the one-shot print
/// heuristic). The single-turn `ok` session's FIRST result (index 0, queued
/// omitted, affectsCompletion=false) settles the root turn — the live sdk case
/// that previously stayed "working" forever.
#[test]
fn first_result_of_a_single_turn_session_settles_the_root() {
    let ok = result_natives(
        include_str!("../../remuda-testing/fixtures/scripts/ok.jsonl"),
        DriverKind::ClaudeSdk,
    );
    assert_eq!(ok.len(), 1);
    assert!(
        settles_root_turn(&ok[0]),
        "the first (only) result must carry settledRootTurn"
    );
    assert!(
        !ok[0].affects_completion,
        "index 0 still has the print heuristic false; settlement is separate"
    );

    // The shared print-carrier mapper makes the same decision.
    let ok_print = result_natives(
        include_str!("../../remuda-testing/fixtures/scripts/ok.jsonl"),
        DriverKind::ClaudePrint,
    );
    assert!(settles_root_turn(&ok_print[0]));
}

/// In a multi-turn session each turn's result ends that turn (the next turn is
/// already driven by its own turn_started), so both twoturn results settle —
/// including index 0.
#[test]
fn every_result_of_a_multi_turn_session_settles_its_turn() {
    let two = result_natives(
        include_str!("../../remuda-testing/fixtures/scripts/twoturn.jsonl"),
        DriverKind::ClaudeSdk,
    );
    assert_eq!(two.len(), 2);
    assert!(settles_root_turn(&two[0]), "turn 1 settles");
    assert!(settles_root_turn(&two[1]), "turn 2 settles");
}

/// A background Workflow emits an intermediate result (index 0) while the
/// workflow is open — that one does NOT settle the root; the final result
/// (index 1) does. This is the one shape `result_index` still gates.
#[test]
fn a_workflow_intermediate_result_does_not_settle_until_its_final() {
    // Drive the workflow script (system/task_started local_workflow ->
    // result 0 -> result 1) through the mapper.
    let wf = result_natives(
        include_str!("../../remuda-testing/fixtures/scripts/workflow.jsonl"),
        DriverKind::ClaudeSdk,
    );
    assert_eq!(wf.len(), 2);
    assert!(
        !settles_root_turn(&wf[0]),
        "the workflow's index-0 result is intermediate"
    );
    assert!(
        settles_root_turn(&wf[1]),
        "the workflow's final result settles"
    );
}

fn result_lines(source: &str, driver: DriverKind) -> Vec<remuda_protocol::NativeLifecycle> {
    result_natives(source, driver)
}

/// r3 item 2: the REAL stopped-workflow capture. The workflow's terminal
/// task_notification (status=stopped, after a task_updated killed patch)
/// arrives BEFORE the only result, which carries the process-global
/// result_index 0 and queued 0. The terminal notification — not the index —
/// is what closes the workflow, so that single result settles the root turn.
/// Pre-r3 the counter-based bookkeeping kept the root "working" forever.
#[test]
fn the_stopped_workflow_canary_settles_on_its_terminal_notification() {
    let results = result_lines(
        include_str!("../../remuda-testing/fixtures/claude/claude-workflow-canary-1.jsonl"),
        DriverKind::ClaudeSdk,
    );
    assert_eq!(results.len(), 1, "the canary has exactly one result");
    assert!(
        settles_root_turn(&results[0]),
        "the post-notification result (index 0) settles the root turn"
    );
}

/// r3 item 3: with TWO workflows owned by one turn, closing the first one
/// does not settle: an intervening result stays intermediate until the SECOND
/// workflow's terminal notification; only the last result settles.
#[test]
fn two_open_workflows_settle_only_after_both_terminate() {
    let results = result_lines(
        include_str!("../../remuda-testing/fixtures/scripts/workflow-two.jsonl"),
        DriverKind::ClaudeSdk,
    );
    assert_eq!(results.len(), 3);
    assert!(!settles_root_turn(&results[0]), "both workflows open");
    assert!(
        !settles_root_turn(&results[1]),
        "A terminated but B still open: the first close must not settle"
    );
    assert!(settles_root_turn(&results[2]), "both terminated");
}

/// r3 item 1: result_index is process-global and grows across turns, so a
/// workflow intermediate in a LATER root turn can carry a NONZERO index. The
/// index must not settle it; the owning turn's open workflow does.
#[test]
fn a_later_turn_workflow_intermediate_with_nonzero_index_does_not_settle() {
    let results = result_lines(
        include_str!("../../remuda-testing/fixtures/scripts/workflow-later-turn.jsonl"),
        DriverKind::ClaudeSdk,
    );
    assert_eq!(results.len(), 3);
    // Turn 1 (index 0) settles normally.
    assert!(settles_root_turn(&results[0]));
    // Turn 2's workflow intermediate has index 1 (nonzero) but must NOT settle.
    assert!(
        !settles_root_turn(&results[1]),
        "a nonzero result_index is not evidence of settlement while the workflow is open"
    );
    // After the terminal notification (status=stopped), index 2 settles.
    assert!(settles_root_turn(&results[2]));
}

/// r3 item 4: an older buffered result mapped AFTER a newer turn_started was
/// published must settle the OLDER outstanding turn, not the newer input.
///
/// The mapper tracks locally-written turns as a FIFO: the first result pops
/// the front turn and settles the root only when NO newer turn remains
/// outstanding. Here two prompts are written (A then B), the result for A is
/// mapped after B's turn_started: it cannot idle B; B's own result then
/// settles.
#[test]
fn an_older_buffered_result_cannot_settle_a_newer_outstanding_input() {
    let mut mapper = StdoutMapper::new(DriverKind::ClaudeSdk, SESSION);
    let _started_a = mapper.turn_started().expect("turn A started");
    let _started_b = mapper.turn_started().expect("turn B started");

    // A result line for turn A mapped from the reader NOW — after B's start
    // was published (the buffer/reorder race).
    let result_a = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": "A done",
        "stop_reason": "end_turn",
        "session_id": SESSION,
        "result_index": 0,
    });
    let obs_a = mapper.map(result_a).expect("map result A");
    let native_a = obs_a
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::Lifecycle(p) => match p.as_ref() {
                LifecyclePayload::Native(n) if n.native_name == "result" => Some(n),
                _ => None,
            },
            _ => None,
        })
        .expect("result lifecycle A");
    assert!(
        !settles_root_turn(native_a),
        "turn B is still outstanding: A's result must not idle the root"
    );

    // B's own result settles.
    let result_b = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": "B done",
        "stop_reason": "end_turn",
        "session_id": SESSION,
        "result_index": 1,
    });
    let obs_b = mapper.map(result_b).expect("map result B");
    let settles_b = obs_b.iter().any(|o| match &o.body {
        ObservationPayload::Lifecycle(p) => match p.as_ref() {
            LifecyclePayload::Native(n) => {
                n.native_name == "result"
                    && n.related_ids.get("settledRootTurn").map(String::as_str) == Some("true")
            }
            _ => false,
        },
        _ => false,
    });
    assert!(
        settles_b,
        "the last outstanding turn's result settles the root"
    );
}
