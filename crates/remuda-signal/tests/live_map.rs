//! Payload-level tests for the live layer against the *recorded* hook JSON
//! (`crates/remuda-testing/fixtures/hooks/claude-hook-session.jsonl`).
//!
//! These shapes are replayed, not authored: the recorded session runs one
//! `Bash` tool, a permission prompt, a streamed one-word reply and a `Stop`.
//! The tests pin the five safety rules in live-view design §2.2 and the
//! "exactly two content events for one tool, no matter how long it runs"
//! contract that closes the measured 20.6 s dead window (design §4.1).
//!
//! The phase vocabulary rides as tags on the raw lifecycle observation
//! (`LiveFold::related`); tool content comes back as gated extra payloads
//! (`LiveFold::extras`).

use remuda_protocol::{
    Knowledge, MutationOperation, ObservationPayload, ResultStage, ToolCallState, ToolOutcome,
};
use remuda_signal::bus::has_phase;
use remuda_signal::{
    BusContext, HookEnvelope, HookEvent, LiveFold, LiveState, SignalBus, map_event, tool_node_id,
};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::mpsc;

const TOOL_ID: &str = "call_ddhgquenm9eoydh6dlgcye7g";
const SCOPE: &str = "ins_test_scope";
const NOW: &str = "2026-09-15T00:00:00.000Z";

/// Load the recorded session as decoded hook events in arrival order.
fn recorded() -> Vec<HookEvent> {
    remuda_testing::hook_session_fixture()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("fixture line is JSON");
            HookEvent::from_envelope(HookEnvelope {
                credential: "fixture".into(),
                event: value["event"].as_str().expect("event name").to_owned(),
                ppid: i32::try_from(value["ppid"].as_i64().expect("ppid")).expect("ppid fits"),
                payload: value["payload"].clone(),
            })
        })
        .collect()
}

fn event(name: &str, ppid: i32, payload: serde_json::Value) -> HookEvent {
    HookEvent {
        name: name.into(),
        ppid,
        payload,
    }
}

/// The result of folding an event stream in order.
struct Folded {
    /// One entry per event that opened a phase: the phase spelling.
    phases: Vec<String>,
    /// Every extra content payload the gate admitted.
    extras: Vec<ObservationPayload>,
}

fn fold(events: &[HookEvent], allowed: bool) -> Folded {
    let mut state = LiveState::new(SCOPE);
    let mut phases = Vec::new();
    let mut extras = Vec::new();
    for event in events {
        let mapped = map_event(event);
        let fold = state.observe(event, &mapped, NOW, allowed);
        if fold.transition {
            phases.push(fold.related["phase"].clone());
        }
        extras.extend(fold.extras);
    }
    Folded { phases, extras }
}

/// The phase tags attached to one event.
fn fold_one(event: &HookEvent, allowed: bool) -> std::collections::BTreeMap<String, String> {
    let mut state = LiveState::new(SCOPE);
    let mapped = map_event(event);
    state.observe(event, &mapped, NOW, allowed).related
}

fn tool_calls(payloads: &[ObservationPayload]) -> Vec<&remuda_protocol::ToolCallPayload> {
    payloads
        .iter()
        .filter_map(|payload| match payload {
            ObservationPayload::ToolCall(call) => Some(call.as_ref()),
            _ => None,
        })
        .collect()
}

fn tool_results(payloads: &[ObservationPayload]) -> Vec<&remuda_protocol::ToolResultPayload> {
    payloads
        .iter()
        .filter_map(|payload| match payload {
            ObservationPayload::ToolResult(result) => Some(result.as_ref()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_recorded_session_unfolds_into_exactly_the_live_phase_sequence() {
    let folded = fold(&recorded(), true);
    assert_eq!(
        folded.phases,
        vec![
            "prompt-accepted",
            "tool-started",
            "blocked",
            "tool-finished",
            // One tag for the streaming episode (the first chunk); later
            // chunks re-tag the raw delivery but do not open a new phase.
            "text-streaming",
            "turn-ended",
        ],
        "one phase per real transition: PostToolBatch adds nothing, \
         SessionStart/SessionEnd are not phases"
    );
}

#[test]
fn the_recorded_tool_runs_then_finishes_on_one_derived_node() {
    let folded = fold(&recorded(), true);
    let calls = tool_calls(&folded.extras);
    let results = tool_results(&folded.extras);
    assert_eq!(calls.len(), 1, "one open, not one per hook re-fire");
    assert_eq!(results.len(), 1, "PostToolBatch must not add a result");

    let call = calls[0];
    assert_eq!(call.state, ToolCallState::Running, "the zero-emitter state");
    assert_eq!(call.mutation.operation, MutationOperation::Open);
    assert_eq!(call.mutation.revision.0, 1);
    assert_eq!(
        call.tool_call_id,
        tool_node_id(SCOPE, TOOL_ID).expect("derived node id")
    );
    let Knowledge::Known { value: input } = &call.input else {
        panic!("the hook carries the real tool input");
    };
    assert_eq!(input["command"], "echo remuda-hook-probe");

    let result = results[0];
    assert_eq!(result.stage, ResultStage::Final);
    assert_eq!(result.outcome, ToolOutcome::Succeeded);
    assert_eq!(result.mutation.operation, MutationOperation::Close);
    // The mutation chain the web assembler's newerMutation guard upgrades in
    // place: close rev 2 builds on open rev 1 of the same node.
    assert_eq!(result.mutation.revision.0, 2);
    assert_eq!(result.mutation.base_revision.as_ref().unwrap().0, 1);
    assert_eq!(result.tool_call_id, call.tool_call_id);
}

#[test]
fn however_long_a_tool_runs_it_is_exactly_two_journal_content_events() {
    // This is the 20.6 s window, priced: one Running open at PreToolUse, one
    // Final close at PostToolUse, and nothing — no spinner frame, no elapsed
    // tick — may ever appear between them. The elapsed timer lives in the
    // browser, off the wire.
    let events = vec![
        event(
            "PreToolUse",
            4242,
            serde_json::json!({"tool_use_id":"call_sleep","tool_name":"Bash","tool_input":{"command":"sleep 20"}}),
        ),
        event(
            "PostToolUse",
            4242,
            serde_json::json!({"tool_use_id":"call_sleep","tool_name":"Bash","tool_response":{"stdout":""}}),
        ),
    ];
    // The fold is input-driven arithmetic; a real 20 s gap produces exactly
    // what a 10 ms gap does. Sleep briefly just to prove nothing is polling.
    std::thread::sleep(Duration::from_millis(10));
    let folded = fold(&events, true);
    assert_eq!(folded.extras.len(), 2, "start and finish only");
    assert!(matches!(folded.extras[0], ObservationPayload::ToolCall(_)));
    assert!(matches!(
        folded.extras[1],
        ObservationPayload::ToolResult(_)
    ));
}

#[test]
fn duplicate_or_reattempted_hooks_never_double_a_transition() {
    let events = vec![
        event(
            "PreToolUse",
            1,
            serde_json::json!({"tool_use_id":"call_x","tool_name":"Read","tool_input":{}}),
        ),
        // Hooks can re-fire.
        event(
            "PreToolUse",
            1,
            serde_json::json!({"tool_use_id":"call_x","tool_name":"Read","tool_input":{}}),
        ),
        event(
            "PostToolUse",
            1,
            serde_json::json!({"tool_use_id":"call_x","tool_response":{}}),
        ),
        // PostToolUse always precedes the batch summary.
        event(
            "PostToolBatch",
            1,
            serde_json::json!({"tool_calls":[{"tool_use_id":"call_x","tool_response":{}}]}),
        ),
    ];
    let folded = fold(&events, true);
    assert_eq!(tool_calls(&folded.extras).len(), 1);
    assert_eq!(tool_results(&folded.extras).len(), 1);
    assert_eq!(
        folded.phases,
        vec!["tool-started", "tool-finished"],
        "the duplicate open and the batch summary are not new transitions"
    );
}

#[test]
fn a_batch_only_finish_closes_revision_one_without_a_phantom_open() {
    // A batch summary arriving for a call whose PreToolUse we never saw (e.g.
    // joined mid-turn) still reports exactly once, as a rev-1 close.
    let events = vec![event(
        "PostToolBatch",
        1,
        serde_json::json!({"tool_calls":[{"tool_use_id":"call_orphan","tool_name":"Bash","tool_response":{"stdout":"ok"}}]}),
    )];
    let folded = fold(&events, true);
    assert_eq!(tool_calls(&folded.extras).len(), 0);
    let results = tool_results(&folded.extras);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].mutation.revision.0, 1);
    assert_eq!(results[0].mutation.base_revision, None);
    assert_eq!(folded.phases, vec!["tool-finished"]);
}

#[test]
fn streamed_text_is_one_phase_for_the_whole_stream_and_tagged_partial() {
    let events = vec![
        event(
            "MessageDisplay",
            1,
            serde_json::json!({"message_id":"m1","index":0,"final":false,"delta":"one\ntwo\n"}),
        ),
        event(
            "MessageDisplay",
            1,
            serde_json::json!({"message_id":"m1","index":1,"final":false,"delta":"three\n"}),
        ),
        event(
            "MessageDisplay",
            1,
            serde_json::json!({"message_id":"m1","index":2,"final":true,"delta":"four"}),
        ),
    ];
    // The stream opens exactly one phase…
    let folded = fold(&events, true);
    assert_eq!(folded.phases, vec!["text-streaming"]);
    // …but every raw chunk carries the phase tag so a tailer reading any chunk
    // in isolation still classifies it.
    let third = fold_one(&events[2], true);
    assert_eq!(third["completeness"], "partial");
    assert_eq!(third["messageId"], "m1");
    assert_eq!(third["chunkIndex"], "2");
    assert_eq!(third["final"], "true");
}

#[test]
fn turn_live_carries_the_since_anchor_and_fidelity_tags() {
    let tags = fold_one(
        &event(
            "UserPromptSubmit",
            4242,
            serde_json::json!({"prompt_id":"p1","prompt":"hi","session_id":"s1"}),
        ),
        true,
    );
    assert_eq!(tags["phase"], "prompt-accepted");
    assert_eq!(tags["since"], NOW, "elapsed anchors here, nowhere else");
    assert_eq!(tags["provision"], "native");
    assert_eq!(tags["tier"], "hook");
    assert_eq!(tags["completeness"], "structured");
    assert_eq!(tags["promptId"], "p1");
}

#[test]
fn stop_and_stop_failure_report_their_outcomes_but_never_a_tool_phase() {
    let completed = fold_one(&event("Stop", 1, serde_json::json!({})), true);
    assert_eq!(completed["phase"], "turn-ended");
    assert_eq!(completed["outcome"], "completed");
    let failed = fold_one(&event("StopFailure", 1, serde_json::json!({})), true);
    assert_eq!(failed["phase"], "turn-ended");
    assert_eq!(failed["outcome"], "failed");
}

#[test]
fn subagent_stop_session_events_and_unknown_hooks_have_no_phase() {
    // Rule 7: SubagentStop fires with no subagent at all.
    for name in [
        "SubagentStop",
        "SessionStart",
        "SessionEnd",
        "PreCompact",
        "",
    ] {
        let tags = fold_one(&event(name, 1, serde_json::json!({})), true);
        assert!(
            !tags.contains_key("phase"),
            "{name:?} must not open a phase"
        );
    }
}

#[test]
fn thinking_and_tool_output_are_not_emitted_for_claude() {
    // §0.3: no thinking channel. §2.3: no live tool-output channel. The
    // variants exist for file/RPC adapters; the claude hook mapper must never
    // produce them, so the UI cannot show confidence Remuda does not have.
    let folded = fold(&recorded(), true);
    assert!(!folded.phases.contains(&"thinking".to_string()));
    assert!(!folded.phases.contains(&"tool-output".to_string()));
}

#[test]
fn two_prompts_sharing_one_prompt_id_stay_two_acceptances() {
    // prompt_id groups; it must never dedupe (D-028 §14 risk 6: queued prompts
    // share one id).
    let events = vec![
        event(
            "UserPromptSubmit",
            1,
            serde_json::json!({"prompt_id":"shared"}),
        ),
        event(
            "UserPromptSubmit",
            1,
            serde_json::json!({"prompt_id":"shared"}),
        ),
    ];
    assert_eq!(
        fold(&events, true).phases,
        vec!["prompt-accepted", "prompt-accepted"]
    );
}

#[test]
fn an_interrupted_tool_response_cancels_the_result() {
    let events = vec![
        event(
            "PreToolUse",
            1,
            serde_json::json!({"tool_use_id":"call_c","tool_name":"Bash","tool_input":{}}),
        ),
        event(
            "PostToolUse",
            1,
            serde_json::json!({"tool_use_id":"call_c","tool_response":{"interrupted":true,"stdout":""}}),
        ),
    ];
    let folded = fold(&events, true);
    let results = tool_results(&folded.extras);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, ToolOutcome::Cancelled);
}

#[test]
fn a_foreign_ppid_gets_lifecycle_phases_but_never_content() {
    // Content is gated; lifecycle is raw evidence and stays ungated. Feeding
    // the pure fold with `allowed = false` is exactly the bus's owns(ppid)
    // decision for an unbound pid.
    let events = vec![
        event(
            "PreToolUse",
            99,
            serde_json::json!({"tool_use_id":"call_f","tool_name":"Read","tool_input":{"file_path":"/etc/passwd"}}),
        ),
        event(
            "PostToolUse",
            99,
            serde_json::json!({"tool_use_id":"call_f","tool_response":{"stdout":"secret"}}),
        ),
    ];
    let folded = fold(&events, false);
    assert_eq!(folded.phases, vec!["tool-started", "tool-finished"]);
    assert!(tool_calls(&folded.extras).is_empty(), "no injectable call");
    assert!(
        tool_results(&folded.extras).is_empty(),
        "no injectable result"
    );
}

fn bus_context() -> BusContext {
    BusContext {
        instance_id: remuda_protocol::InstanceId::new(),
        host_id: remuda_protocol::HostId::new(),
        journal_id: remuda_protocol::Id::new("obj").unwrap(),
        run_id: remuda_protocol::RunId::new(),
        driver_kind: remuda_protocol::DriverKind::ShellPty,
        adapter_version: "test".into(),
    }
}

async fn drain(
    rx: &mut mpsc::Receiver<remuda_protocol::Observation>,
) -> Vec<remuda_protocol::Observation> {
    tokio::task::yield_now().await;
    let mut out = Vec::new();
    while let Ok(observation) = rx.try_recv() {
        out.push(observation);
    }
    out
}

#[tokio::test]
async fn the_bus_gate_binds_content_to_the_session_start_agent() {
    let (tx, mut rx) = mpsc::channel(128);
    let bus = SignalBus::new(bus_context(), tx, Arc::new(AtomicU64::new(0)));
    // Foreground claude binds pid 4242.
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "SessionStart".into(),
        ppid: 4242,
        payload: serde_json::json!({"session_id":"s-bound","transcript_path":"/w/s.jsonl"}),
    })
    .await;
    drain(&mut rx).await;

    // A second claude in a promoted terminal shares the socket.
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "PreToolUse".into(),
        ppid: 7777,
        payload: serde_json::json!({"tool_use_id":"call_inject","tool_name":"Write","tool_input":{"content":"x"}}),
    })
    .await;
    let foreign = drain(&mut rx).await;
    // Exactly one observation: the raw PreToolUse, phase-tagged but contentless.
    assert_eq!(foreign.len(), 1, "lifecycle only, never a second card");
    assert!(
        has_phase(&foreign[0].body, "tool-started"),
        "phase survives"
    );
    assert!(
        !matches!(foreign[0].body, ObservationPayload::ToolCall(_)),
        "a foreign agent must not draw a tool card on the bound instance"
    );

    // The bound agent's own tool goes through: raw phase + the Running card.
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "PreToolUse".into(),
        ppid: 4242,
        payload: serde_json::json!({"tool_use_id":"call_own","tool_name":"Read","tool_input":{}}),
    })
    .await;
    let owned = drain(&mut rx).await;
    assert!(
        owned
            .iter()
            .any(|o| matches!(o.body, ObservationPayload::ToolCall(_))),
        "the bound agent's content is journaled"
    );
}

#[tokio::test]
async fn before_any_session_start_everything_is_lifecycle_only() {
    // No binding at all: the gate cannot identify an owner, so content waits.
    let (tx, mut rx) = mpsc::channel(64);
    let bus = SignalBus::new(bus_context(), tx, Arc::new(AtomicU64::new(0)));
    bus.handle(HookEnvelope {
        credential: "c".into(),
        event: "PreToolUse".into(),
        ppid: 4242,
        payload: serde_json::json!({"tool_use_id":"call_early","tool_name":"Bash","tool_input":{}}),
    })
    .await;
    let observations = drain(&mut rx).await;
    assert_eq!(observations.len(), 1, "one raw lifecycle observation");
    assert!(
        has_phase(&observations[0].body, "tool-started"),
        "lifecycle phase survives with no binding"
    );
    assert!(
        !matches!(observations[0].body, ObservationPayload::ToolCall(_)),
        "an unbound ppid yields lifecycle-only"
    );
}

#[test]
fn the_raw_lifecycle_observation_is_where_the_phase_tags_land() {
    // The raw observation keeps its native name (so hook_activity and the
    // message fold keep working) and gains the phase tags.
    let event = event("UserPromptSubmit", 1, serde_json::json!({"prompt_id":"p1"}));
    let mut state = LiveState::new(SCOPE);
    let mapped = map_event(&event);
    let fold: LiveFold = state.observe(&event, &mapped, NOW, true);
    assert!(fold.related.contains_key("phase"));
    assert!(
        fold.extras.is_empty(),
        "a prompt produces phase tags, no content"
    );
}
