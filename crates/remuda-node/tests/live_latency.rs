//! Node-side latency budgets for the live layer (live-view design §4.1).
//!
//! The clock starts where the relay hands the payload to the bus — the first
//! instant Remuda could possibly know — and stops when the derived
//! observation is committed to the journal channel. The fold is in-process
//! arithmetic (the message fold measured 0 ms in `hook_latency.rs`), so these
//! are p99-style 25 ms guards against a regression that adds IO to the path,
//! not benchmarks of the machine.
//!
//! Also here: the convergence guarantee that makes the upgrade flicker-free —
//! the hook relay and the transcript tailer derive the *same* node id for the
//! *same* native `tool_use_id`, asserted on the recorded transcript fixture.

use remuda_journal::{MapContext, NativeIds, digest_of, map_claude_line};
use remuda_protocol::{
    DriverKind, FileCursor, HostId, Id, InstanceId, MutationOperation, Observation,
    ObservationPayload, ResultStage, RunId, SourceChannel, ToolCallState, ToolOutcome, U64,
};
use remuda_signal::bus::{has_phase, native_lifecycle};
use remuda_signal::{
    BusContext, HookEnvelope, HookEvent, LiveState, SignalBus, map_event, tool_node_id,
};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;
use tokio::sync::mpsc;

/// The §4.1 fold budget.
const FOLD_BUDGET_MS: u128 = 25;
/// The original D-028 P3 outer guard, retained for the message path.
const MESSAGE_OUTER_BUDGET_MS: u128 = 300;
const PPID: i32 = 4242;
const NATIVE_TOOL: &str = "toolu_vrtx_01AvYSRwRhJhyF1y7iKpQPdu";

fn bus_context(instance: InstanceId) -> BusContext {
    BusContext {
        instance_id: instance,
        host_id: HostId::new(),
        journal_id: Id::new("obj").unwrap(),
        run_id: RunId::new(),
        driver_kind: DriverKind::ShellPty,
        adapter_version: "test".into(),
    }
}

fn envelope(event: &str, payload: serde_json::Value) -> HookEnvelope {
    HookEnvelope {
        credential: "fixture".into(),
        event: event.into(),
        ppid: PPID,
        payload,
    }
}

async fn drain(rx: &mut mpsc::Receiver<Observation>) -> Vec<Observation> {
    // The raw lifecycle is always emitted first; yield once so every second
    // emission from the same `handle` has landed.
    tokio::task::yield_now().await;
    let mut out = Vec::new();
    while let Ok(observation) = rx.try_recv() {
        out.push(observation);
    }
    out
}

fn is_tool_call(o: &Observation, state: ToolCallState) -> bool {
    matches!(
        &o.body,
        ObservationPayload::ToolCall(call) if call.state == state
    )
}

fn is_phase(observation: &Observation, phase: &str) -> bool {
    has_phase(&observation.body, phase)
}

fn live_phase<'a>(observations: &'a [Observation], phase: &str) -> Option<&'a Observation> {
    observations.iter().find(|o| is_phase(o, phase))
}

async fn handle_and_time(
    bus: &SignalBus,
    rx: &mut mpsc::Receiver<Observation>,
    event: HookEnvelope,
) -> (Vec<Observation>, u128) {
    let started = Instant::now();
    bus.handle(event).await;
    let observations = drain(rx).await;
    (observations, started.elapsed().as_nanos() / 1_000_000)
}

async fn bound_bus() -> (SignalBus, mpsc::Receiver<Observation>, InstanceId) {
    let instance = InstanceId::new();
    let (tx, mut rx) = mpsc::channel(256);
    let bus = SignalBus::new(
        bus_context(instance.clone()),
        tx,
        Arc::new(AtomicU64::new(0)),
    );
    bus.handle(envelope(
        "SessionStart",
        serde_json::json!({"session_id":"0199a1f0-0000-7000-8000-000000000000","transcript_path":"/w/s.jsonl"}),
    ))
    .await;
    drain(&mut rx).await;
    (bus, rx, instance)
}

#[tokio::test]
async fn pre_tool_use_becomes_a_running_tool_call_inside_the_budget() {
    let (bus, mut rx, _instance) = bound_bus().await;
    while rx.try_recv().is_ok() {}

    let (observations, elapsed_ms) = handle_and_time(
        &bus,
        &mut rx,
        envelope(
            "PreToolUse",
            serde_json::json!({"tool_use_id":"call_sleep","tool_name":"Bash","tool_input":{"command":"sleep 20"}}),
        ),
    )
    .await;
    let call = observations
        .iter()
        .find(|o| is_tool_call(o, ToolCallState::Running))
        .expect("PreToolUse opens a Running tool call");
    let ObservationPayload::ToolCall(call) = &call.body else {
        unreachable!()
    };
    assert_eq!(call.mutation.operation, MutationOperation::Open);
    assert!(
        elapsed_ms <= FOLD_BUDGET_MS,
        "Running state took {elapsed_ms} ms; budget is {FOLD_BUDGET_MS} ms"
    );
    // The phase anchor rides the same handle.
    assert!(live_phase(&observations, "tool-started").is_some());
}

#[tokio::test]
async fn post_tool_use_closes_the_same_node_inside_the_budget() {
    let (bus, mut rx, _instance) = bound_bus().await;
    while rx.try_recv().is_ok() {}
    bus.handle(envelope(
        "PreToolUse",
        serde_json::json!({"tool_use_id":"call_sleep","tool_name":"Bash","tool_input":{"command":"sleep 20"}}),
    ))
    .await;
    let opened = drain(&mut rx).await;
    let open_id = match &opened
        .iter()
        .find(|o| is_tool_call(o, ToolCallState::Running))
        .unwrap()
        .body
    {
        ObservationPayload::ToolCall(call) => call.tool_call_id.clone(),
        _ => unreachable!(),
    };

    let (observations, elapsed_ms) = handle_and_time(
        &bus,
        &mut rx,
        envelope(
            "PostToolUse",
            serde_json::json!({"tool_use_id":"call_sleep","tool_name":"Bash","tool_response":{"stdout":"","exitCode":0},"duration_ms":20000}),
        ),
    )
    .await;
    let result = observations
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("PostToolUse yields a final result");
    assert_eq!(result.stage, ResultStage::Final);
    assert_eq!(result.outcome, ToolOutcome::Succeeded);
    assert_eq!(
        result.tool_call_id, open_id,
        "finish lands on the open node"
    );
    assert_eq!(result.mutation.revision.0, 2);
    assert_eq!(result.mutation.base_revision.as_ref().unwrap().0, 1);
    assert!(
        elapsed_ms <= FOLD_BUDGET_MS,
        "final result took {elapsed_ms} ms; budget is {FOLD_BUDGET_MS} ms"
    );
}

#[tokio::test]
async fn prompt_and_stop_phases_land_inside_the_budget_with_outcomes() {
    let (bus, mut rx, _instance) = bound_bus().await;
    while rx.try_recv().is_ok() {}

    let (prompt_obs, prompt_ms) = handle_and_time(
        &bus,
        &mut rx,
        envelope(
            "UserPromptSubmit",
            serde_json::json!({"prompt_id":"p1","prompt":"run a long thing"}),
        ),
    )
    .await;
    let accepted = live_phase(&prompt_obs, "prompt-accepted").expect("prompt-accepted");
    assert_eq!(
        native_lifecycle(&accepted.body).unwrap().status,
        remuda_protocol::Knowledge::Known {
            value: "working".into()
        }
    );
    assert!(prompt_ms <= FOLD_BUDGET_MS);

    let (stop_obs, stop_ms) = handle_and_time(
        &bus,
        &mut rx,
        envelope("Stop", serde_json::json!({"stop_hook_active":false})),
    )
    .await;
    let ended = live_phase(&stop_obs, "turn-ended").expect("turn-ended");
    assert_eq!(
        native_lifecycle(&ended.body).unwrap().related_ids["outcome"],
        "completed"
    );
    assert!(stop_ms <= FOLD_BUDGET_MS);
}

#[tokio::test]
async fn message_display_stays_readable_inside_the_inner_budget() {
    let (bus, mut rx, _instance) = bound_bus().await;
    while rx.try_recv().is_ok() {}
    let started = Instant::now();
    bus.handle(envelope(
        "MessageDisplay",
        serde_json::json!({"turn_id":"t","message_id":"m1","index":0,"final":true,"delta":"done"}),
    ))
    .await;
    let observations = drain(&mut rx).await;
    let elapsed_ms = started.elapsed().as_nanos() / 1_000_000;

    let source = observations
        .iter()
        .find(|o| remuda_node::message_delta(o).is_some())
        .expect("the raw MessageDisplay delta");
    let delta = remuda_node::message_delta(source).unwrap();
    let mut assembler = remuda_node::MessageAssembler::new();
    let message = assembler.fold(&delta).expect("readable text");
    assert_eq!(
        message
            .blocks
            .iter()
            .find_map(|b| match b {
                remuda_protocol::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .unwrap(),
        "done"
    );
    // One text-streaming phase for the episode, tagged partial.
    let phase = live_phase(&observations, "text-streaming").expect("text-streaming");
    assert_eq!(
        native_lifecycle(&phase.body).unwrap().related_ids["completeness"],
        "partial"
    );
    assert!(elapsed_ms <= FOLD_BUDGET_MS);
    assert!(elapsed_ms <= MESSAGE_OUTER_BUDGET_MS);
}

#[tokio::test]
async fn a_twenty_second_tool_is_exactly_two_journal_events_and_one_phase_pair() {
    // The measured defect: 20.6 s with zero journal events while the TTY
    // pushed 218 frames. The fix prices the gap at two events total; the
    // browser owns the 1 Hz timer, the wire never sees it.
    let (bus, mut rx, _instance) = bound_bus().await;
    while rx.try_recv().is_ok() {}
    bus.handle(envelope(
        "UserPromptSubmit",
        serde_json::json!({"prompt_id":"p1"}),
    ))
    .await;
    drain(&mut rx).await;
    bus.handle(envelope(
        "PreToolUse",
        serde_json::json!({"tool_use_id":"call_sleep","tool_name":"Bash","tool_input":{"command":"sleep 20"}}),
    ))
    .await;
    let at_start = drain(&mut rx).await;
    // No hook fires while the tool runs. The fold is event-driven, so
    // "nothing for 20 s" is "nothing we have to assert" — a spin here would
    // only measure the test harness. A re-fired PreToolUse still journals the
    // raw evidence (tagged with the phase it belongs to) but opens no new card.
    bus.handle(envelope(
        "PreToolUse",
        serde_json::json!({"tool_use_id":"call_sleep","tool_name":"Bash","tool_input":{"command":"sleep 20"}}),
    ))
    .await;
    let duplicate = drain(&mut rx).await;
    assert_eq!(duplicate.len(), 1, "raw evidence re-delivery only");
    assert!(
        duplicate
            .iter()
            .all(|o| !matches!(o.body, ObservationPayload::ToolCall(_))),
        "a duplicate PreToolUse must not open a second card"
    );

    bus.handle(envelope(
        "PostToolUse",
        serde_json::json!({"tool_use_id":"call_sleep","tool_name":"Bash","tool_response":{"stdout":""}}),
    ))
    .await;
    let at_finish = drain(&mut rx).await;
    bus.handle(envelope(
        "PostToolBatch",
        serde_json::json!({"tool_calls":[{"tool_use_id":"call_sleep","tool_response":""}]}),
    ))
    .await;
    let batch = drain(&mut rx).await;
    assert_eq!(batch.len(), 1, "the raw batch summary is one observation");
    assert!(
        !is_phase(&batch[0], "tool-finished"),
        "an already-finished tool's batch summary is not a new transition"
    );
    assert!(
        !matches!(batch[0].body, ObservationPayload::ToolResult(_)),
        "nor a second result"
    );

    let content: Vec<_> = at_start
        .iter()
        .chain(at_finish.iter())
        .filter(|o| {
            matches!(
                o.body,
                ObservationPayload::ToolCall(_) | ObservationPayload::ToolResult(_)
            )
        })
        .collect();
    assert_eq!(
        content.len(),
        2,
        "one Running open, one Final close — that's it"
    );
    assert!(matches!(content[0].body, ObservationPayload::ToolCall(_)));
    assert!(matches!(content[1].body, ObservationPayload::ToolResult(_)));
}

#[tokio::test]
async fn one_phase_transition_emits_at_most_one_live_observation() {
    let (bus, mut rx, _instance) = bound_bus().await;
    while rx.try_recv().is_ok() {}
    // Two identical permission dialogs (Notification follows PermissionRequest)
    // latch one `blocked`.
    for event in ["PermissionRequest", "Notification"] {
        bus.handle(envelope(
            event,
            serde_json::json!({"tool_name":"Write","tool_input":{}}),
        ))
        .await;
    }
    let observations = drain(&mut rx).await;
    assert_eq!(
        observations
            .iter()
            .filter(|o| is_phase(o, "blocked"))
            .count(),
        1,
        "the 10 Hz spinner must never reach the wire; neither may a dialog echo"
    );
}

#[tokio::test]
async fn hook_and_transcript_converge_on_one_tool_node_on_recorded_fixture() {
    // The acceptance fixture: a real recorded 2.1.270 transcript containing
    // the tool_use block and its tool_result.
    let path =
        remuda_testing::fixtures_dir().join("claude-transcript/queue/interactive-session.jsonl");
    let bytes = std::fs::read_to_string(path).expect("recorded transcript fixture");
    let lines: Vec<&str> = bytes
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let assistant_line = lines
        .iter()
        .find(|line| line.contains(&format!("\"id\":\"{NATIVE_TOOL}\"")))
        .expect("the recorded tool_use block");
    let result_line = lines
        .iter()
        .find(|line| line.contains(&format!("\"tool_use_id\":\"{NATIVE_TOOL}\"")))
        .expect("the recorded tool_result record");

    let instance = InstanceId::new();
    let ctx = MapContext::claude_file(
        instance.clone(),
        Id::new("obj").unwrap(),
        HostId::new(),
        "4bd84da7-16cb-430f-ae97-d85f1ea22c1f",
        SourceChannel::Transcript,
    );
    let mut ids = NativeIds::new(instance.as_id().as_str());
    let cursor_for = |line: &str| FileCursor {
        file_identity: Id::new("obj").unwrap(),
        file_generation: U64(1),
        offset: U64(0),
        length: U64(line.len() as u64),
        digest: digest_of(line.as_bytes()),
    };

    let call_envelopes = map_claude_line(
        &ctx,
        &mut ids,
        assistant_line.as_bytes(),
        &cursor_for(assistant_line),
    )
    .expect("map the recorded tool_use");
    let result_envelopes = map_claude_line(
        &ctx,
        &mut ids,
        result_line.as_bytes(),
        &cursor_for(result_line),
    )
    .expect("map the recorded tool_result");

    let transcript_call_id = call_envelopes
        .iter()
        .find_map(|e| match &e.body {
            ObservationPayload::ToolCall(call) => Some(call.tool_call_id.clone()),
            _ => None,
        })
        .expect("transcript tool call");
    let transcript_result_id = result_envelopes
        .iter()
        .find_map(|e| match &e.body {
            ObservationPayload::ToolResult(result) => Some(result.tool_call_id.clone()),
            _ => None,
        })
        .expect("transcript tool result");
    assert_eq!(
        transcript_call_id, transcript_result_id,
        "transcript call and result already share one node"
    );

    // The same native id through the hook fold derives the very same node.
    let expected =
        tool_node_id(instance.as_id().as_str(), NATIVE_TOOL).expect("deterministic derived id");
    assert_eq!(transcript_call_id, expected);

    let event = HookEvent {
        name: "PreToolUse".into(),
        ppid: PPID,
        payload: serde_json::json!({"tool_use_id":NATIVE_TOOL,"tool_name":"Bash","tool_input":{"command":"sleep 20"}}),
    };
    let mapped = map_event(&event);
    let mut live = LiveState::new(instance.as_id().as_str());
    let fold = live.observe(&event, &mapped, "2026-09-13T18:23:41.700Z", true);
    let hook_call_id = fold
        .extras
        .iter()
        .find_map(|payload| match payload {
            ObservationPayload::ToolCall(call) => Some(call.tool_call_id.clone()),
            _ => None,
        })
        .expect("hook tool call");
    assert_eq!(
        hook_call_id, transcript_call_id,
        "the transcript upgrades the hook's node in place instead of a second card"
    );
}
