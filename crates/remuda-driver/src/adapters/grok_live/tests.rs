//! Tests for the file-tier live fold, driven against the captured grok 1.0.30
//! fixture in the adapter's real drain order (updates.jsonl fully before
//! events.jsonl, as the first poll after attach replays it) and synthesized
//! frames labelled [U] for the shapes the fixture never captures.

use super::*;
use crate::adapters::{AdapterHome, GrokAdapter};
use remuda_protocol::{
    HostId, Id, InstanceId, InteractionCarrier, InteractionRequest, InteractionResolutionReason,
    InteractionState, Knowledge, LifecycleEntity, LifecyclePayload, LifecycleTopic, MessageRole,
    NativeLifecycle, ObservationPayload, QuestionInput, RunId, Severity, ToolCallState,
    ToolOutcome,
};
use serde_json::{Value, json};
use std::fs;

const REAL_UPDATES: &str = include_str!("../../../tests/fixtures/grok/tui-updates.jsonl");
const REAL_EVENTS: &str = include_str!("../../../tests/fixtures/grok/tui-events.jsonl");
const REAL_REGISTRY: &str = include_str!("../../../tests/fixtures/grok/active-sessions.json");
const SESSION_ID: &str = "01a09c24-46ef-7a03-9c89-88f1bc00bd0c";

/// Native prompt ids of the fixture's seven turns, in turn order.
const PROMPTS: &[&str] = &[
    "a759f86b-bfcd-4fbf-b8e1-e1180acfc33f",
    "d213ff8e-f720-4503-8299-c15d208d9e14",
    "15d7bdf4-ff1b-4a72-8e8e-24b241845a68",
    "735123be-3539-4593-86e1-c99746db34d6",
    "9295dfdc-6e81-4ee0-8e77-3c940dd625d1",
    "96afbba8-2d51-45ab-8175-b3bee609a330",
    "6e95840b-9483-4976-aa1f-6c3054093786",
];

fn identity() -> LiveIdentity {
    LiveIdentity {
        instance_id: InstanceId::new(),
        host_id: HostId::new(),
        run_id: RunId::new(),
    }
}

fn fixed_now() -> Timestamp {
    Timestamp::try_from("2026-09-19T00:00:00.000Z".to_owned()).unwrap()
}

/// Write the complete captured session and bind a [`GrokLive`] over it.
fn replay_fixture() -> (tempfile::TempDir, GrokLive) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let encoded = crate::grok_session::encode_session_cwd(&cwd);
    let session_dir = dir.path().join("sessions").join(&encoded).join(SESSION_ID);
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("updates.jsonl"), REAL_UPDATES).unwrap();
    fs::write(session_dir.join("events.jsonl"), REAL_EVENTS).unwrap();
    fs::write(session_dir.join("usage.json"), "{}\n").unwrap();
    let mut registry: Value = serde_json::from_str(REAL_REGISTRY.trim()).unwrap();
    registry[0]["cwd"] = Value::String(cwd.to_string_lossy().into_owned());
    registry[0]["pid"] = Value::Number(24069u64.into());
    fs::write(
        dir.path().join("active_sessions.json"),
        serde_json::to_vec(&registry).unwrap(),
    )
    .unwrap();
    let live = GrokLive::new(
        GrokAdapter::new(AdapterHome {
            home: dir.path().to_path_buf(),
            cwd,
            pid: Some(24069),
        }),
        identity(),
    );
    (dir, live)
}

/// Drain the already-written fixture the way production does on first attach:
/// one discover-and-drain poll reads all of `updates.jsonl` before any of
/// `events.jsonl` (the adapter drains updates first); a second poll is empty.
fn drain_fixture(live: &mut GrokLive) -> Vec<AdapterObservation> {
    let mut all = live.poll().unwrap();
    all.extend(live.poll().unwrap());
    all
}

/// (native_name, phase, promptId) for every Turn-topic native lifecycle
/// carrying a phase tag, in emission order.
fn phase_events(observed: &[AdapterObservation]) -> Vec<(String, String, Option<String>)> {
    observed
        .iter()
        .filter_map(|observation| {
            let ObservationPayload::Lifecycle(box_payload) = &observation.payload else {
                return None;
            };
            let LifecyclePayload::Native(native) = box_payload.as_ref() else {
                return None;
            };
            if native.topic != LifecycleTopic::Turn {
                return None;
            }
            native.related_ids.get("phase").map(|phase| {
                (
                    native.native_name.clone(),
                    phase.clone(),
                    native.related_ids.get("promptId").cloned(),
                )
            })
        })
        .collect()
}

fn requested_interactions(observed: &[AdapterObservation]) -> Vec<Interaction> {
    observed
        .iter()
        .filter_map(|observation| match &observation.payload {
            ObservationPayload::InteractionRequested(payload) => Some(payload.interaction.clone()),
            _ => None,
        })
        .collect()
}

fn resolved_entities(observed: &[AdapterObservation]) -> Vec<Interaction> {
    observed
        .iter()
        .filter_map(|observation| {
            let ObservationPayload::Lifecycle(box_payload) = &observation.payload else {
                return None;
            };
            match box_payload.as_ref() {
                LifecyclePayload::Entity(entity) => match &entity.entity_value {
                    LifecycleEntity::Interaction(interaction) => Some((**interaction).clone()),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect()
}

#[test]
fn replay_anchors_every_prompt_on_the_user_chunk_before_the_boundaries() {
    let (_dir, mut live) = replay_fixture();
    let observed = drain_fixture(&mut live);
    let phases = phase_events(&observed);
    // Seven prompt-accepted anchors (one per turn), and in the real drain
    // order they ride synthesized turn.live lifecycles — updates drain before
    // events, so every user chunk precedes every turn_started boundary.
    let accepted: Vec<_> = phases
        .iter()
        .filter(|(_, phase, _)| phase == "prompt-accepted")
        .collect();
    assert_eq!(accepted.len(), 7, "one anchor per fixture turn");
    assert!(accepted.iter().all(|(name, _, _)| name == TURN_LIVE_NAME));
    // The seven turn_started boundaries still journal for the activity fold,
    // but carry no phase tag: the anchor was already emitted.
    let tagged_starts = observed.iter().filter(|observation| {
        let ObservationPayload::Lifecycle(box_payload) = &observation.payload else {
            return false;
        };
        matches!(box_payload.as_ref(), LifecyclePayload::Native(native)
            if native.native_name == "turn_started"
                && native.related_ids.contains_key("phase"))
    });
    assert_eq!(tagged_starts.count(), 0);
}

#[test]
fn turn_zero_runs_think_tool_think_text_with_reentry() {
    // Defect 1: update frame 9 is a second agent_thought_chunk emitted AFTER
    // the tool's completed frame. Thinking must re-open as a new episode.
    // Boundaries drain after every turn's content, so turn 0 is isolated by
    // its prompt id plus its positionally-first anchor.
    let (_dir, mut live) = replay_fixture();
    let observed = drain_fixture(&mut live);
    let mut tagged = phase_events(&observed);
    let anchor = tagged
        .iter()
        .position(|(_, phase, _)| phase == "prompt-accepted")
        .unwrap();
    let turn_zero: Vec<_> = tagged
        .iter()
        .enumerate()
        .filter(|(_, (_, _, prompt))| prompt.as_deref() == Some(PROMPTS[0]))
        .map(|(_, (name, phase, _))| (name.as_str(), phase.as_str()))
        .collect();
    // Prepend turn 0's positionally-first anchor.
    let mut expected = vec![("turn.live", "prompt-accepted")];
    expected.extend(turn_zero);
    assert_eq!(
        expected,
        vec![
            ("turn.live", "prompt-accepted"),
            ("turn.live", "thinking"),
            ("turn.live", "tool-started"),
            ("turn.live", "tool-output"),
            ("turn.live", "tool-finished"),
            ("turn.live", "thinking"),
            ("turn.live", "text-streaming"),
            ("turn_ended", "turn-ended"),
        ]
    );
    // And the anchor consumed was genuinely the first one emitted.
    assert_eq!(tagged.remove(anchor).1, "prompt-accepted");
}

#[test]
fn every_turn_gets_its_own_thinking_and_text_streaming_with_its_prompt_id() {
    // Defect 2: a full first-poll replay batches all turns before their
    // boundaries; the stream phases must be per-turn, not once-globally.
    let (_dir, mut live) = replay_fixture();
    let observed = drain_fixture(&mut live);
    for (index, prompt) in PROMPTS.iter().enumerate() {
        // Only lifecycles actually tagged with this turn's prompt id.
        let mut phases: Vec<_> = phase_events(&observed)
            .into_iter()
            .filter(|(_, _, tagged)| tagged.as_deref() == Some(*prompt))
            .map(|(_, phase, _)| phase)
            .collect();
        let expected_text = index != 3 && index != 5;
        assert!(phases.contains(&"thinking".to_owned()), "turn {index}");
        assert_eq!(
            phases.iter().filter(|p| p.as_str() == "thinking").count(),
            if index == 0 || index == 4 { 2 } else { 1 },
            "turn {index} re-enters thinking after its tool"
        );
        assert_eq!(
            phases.contains(&"text-streaming".to_owned()),
            expected_text,
            "turn {index} text streaming presence"
        );
        // The end boundary for the turn carries the turn's own promptId.
        let end_phase = if index == 3 || index == 5 {
            "interrupted"
        } else {
            "turn-ended"
        };
        assert!(
            phases.iter().any(|p| p == end_phase),
            "turn {index} ends {end_phase}: {phases:?}"
        );
        // No tagged lifecycle for another prompt smuggled itself in.
        phases.retain(|p| p != end_phase);
        assert!(
            phases.iter().all(|p| [
                "thinking",
                "text-streaming",
                "tool-started",
                "tool-output",
                "tool-finished",
            ]
            .contains(&p.as_str())),
            "turn {index} phase set {phases:?}"
        );
    }
}

#[test]
fn the_two_cancelled_turns_end_interrupted() {
    let (_dir, mut live) = replay_fixture();
    let observed = drain_fixture(&mut live);
    let interrupted = phase_events(&observed)
        .iter()
        .filter(|(name, phase, _)| name == "turn_ended" && phase == "interrupted")
        .count();
    assert_eq!(interrupted, 2, "ctrl_c and send_now turns");
}

#[test]
fn tool_started_lifecycle_is_immediately_before_its_tool_call() {
    let (_dir, mut live) = replay_fixture();
    let observed = drain_fixture(&mut live);
    let start = observed
        .iter()
        .position(|observation| {
            let ObservationPayload::Lifecycle(box_payload) = &observation.payload else {
                return false;
            };
            matches!(box_payload.as_ref(), LifecyclePayload::Native(native)
                if native.native_name == "turn.live"
                    && native.related_ids.get("phase").map(String::as_str)
                        == Some("tool-started")
                    && native.related_ids.get("toolCallId").map(String::as_str)
                        == Some("call-spike-1789326032369250000"))
        })
        .expect("tool-started lifecycle");
    // phase.ts toolAnchors pairs the start with the next tool_call by seq.
    assert!(matches!(
        &observed[start + 1].payload,
        ObservationPayload::ToolCall(call)
            if call.state == ToolCallState::Proposed
    ));
    assert_eq!(
        observed[start + 1].item_id.as_deref(),
        Some("call-spike-1789326032369250000")
    );
}

#[test]
fn file_tier_phase_tags_have_file_fidelity_and_streaming_completeness() {
    let (_dir, mut live) = replay_fixture();
    let observed = drain_fixture(&mut live);
    let find = |phase: &str| {
        observed.iter().find_map(|observation| {
            let ObservationPayload::Lifecycle(box_payload) = &observation.payload else {
                return None;
            };
            match box_payload.as_ref() {
                LifecyclePayload::Native(native)
                    if native.native_name == "turn.live"
                        && native.related_ids.get("phase").map(String::as_str) == Some(phase) =>
                {
                    Some(native)
                }
                _ => None,
            }
        })
    };
    let thinking = find("thinking").expect("thinking turn.live");
    assert_eq!(
        thinking.related_ids.get("tier").map(String::as_str),
        Some("file")
    );
    assert_eq!(
        thinking.related_ids.get("provision").map(String::as_str),
        Some("native")
    );
    assert_eq!(
        thinking.related_ids.get("completeness").map(String::as_str),
        Some("partial")
    );
    assert!(
        thinking
            .related_ids
            .get("since")
            .is_some_and(|v| !v.is_empty())
    );
    let finished = find("tool-finished").expect("tool-finished turn.live");
    assert_eq!(
        finished.related_ids.get("completeness").map(String::as_str),
        Some("structured")
    );
}

#[test]
fn the_question_request_and_resolution_share_one_id_and_bump_revision() {
    // Defect 3: the node retires the pending row by meta.id.
    let (_dir, mut live) = replay_fixture();
    let observed = drain_fixture(&mut live);
    let requested = requested_interactions(&observed);
    assert_eq!(requested.len(), 1, "exactly one question in the fixture");
    let request = &requested[0];
    assert_eq!(request.kind, remuda_protocol::InteractionKind::Question);
    assert!(request.blocking);
    assert!(
        !request.answerable,
        "the file tier has no answer channel (D4)"
    );
    assert_eq!(request.carrier, InteractionCarrier::NativeTty);
    assert_eq!(request.state, InteractionState::Pending);
    assert_eq!(request.meta.revision.0, 1);
    let InteractionRequest::Question(question) = &request.request else {
        panic!("expected a question request");
    };
    assert_eq!(question.fields.len(), 1);
    let field = &question.fields[0];
    assert_eq!(field.input, QuestionInput::SingleSelect);
    assert_eq!(field.title, "Choose the probe result.");
    let labels: Vec<_> = field.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, vec!["Alpha", "Beta"]);
    assert_eq!(
        field.options[0].description.as_deref(),
        Some("Record Alpha.")
    );

    let resolved = resolved_entities(&observed);
    assert_eq!(resolved.len(), 1);
    let resolution = &resolved[0];
    // The same entity identity, revision 1 → 2.
    assert_eq!(resolution.meta.id, request.meta.id);
    assert_eq!(resolution.meta.revision.0, 2);
    assert_eq!(resolution.state, InteractionState::Resolved);
    assert!(!resolution.blocking);
    let reason = match &resolution.resolution {
        Knowledge::Known { value } => &value.reason,
        _ => panic!("resolution recorded"),
    };
    assert_eq!(*reason, InteractionResolutionReason::Answered);
    let answer = match &resolution.answer {
        Knowledge::Known { value } => value,
        _ => panic!("answer committed"),
    };
    let remuda_protocol::InteractionAnswer::Question(question) = &answer.value else {
        panic!("question answer");
    };
    assert_eq!(
        question.answers.get("q0").unwrap().option_ids,
        vec!["Alpha"]
    );
}

/// Local mirror of the name arms of `remuda_node::signal::file_activity`: a
/// `turn.live` lifecycle the fold mints must fold to no Working/Idle by name.
fn file_activity_name(name: &str) -> Option<&'static str> {
    match name {
        "task_started" | "turn_started" => Some("working"),
        "task_complete" | "turn_aborted" | "turn_ended" => Some("idle"),
        _ => None,
    }
}

#[test]
fn turn_live_lifecycle_is_not_an_activity_edge() {
    assert_eq!(file_activity_name(super::TURN_LIVE_NAME), None);
    assert_eq!(file_activity_name("turn_started"), Some("working"));
    assert_eq!(file_activity_name("turn_ended"), Some("idle"));
}

// -- pure fold tests over synthesized frames [U] ---------------------------

fn turn_observation(name: &str, outcome: Option<&str>) -> AdapterObservation {
    let mut native = Box::new(NativeLifecycle {
        topic: LifecycleTopic::Turn,
        native_name: name.to_owned(),
        native_id: Knowledge::NotApplicable,
        status: Knowledge::Known {
            value: "working".into(),
        },
        related_ids: std::collections::BTreeMap::new(),
        data_ref: None,
        severity: Severity::Info,
        affects_completion: false,
    });
    if let Some(outcome) = outcome {
        native.related_ids.insert("outcome".into(), outcome.into());
    }
    AdapterObservation::structured(ObservationPayload::Lifecycle(Box::new(
        LifecyclePayload::Native(native),
    )))
}

fn thought_observation(turn_id: Option<&str>) -> AdapterObservation {
    let mut observed = AdapterObservation::partial(crate::adapters::thought_chunk(
        Id::new("obj").unwrap(),
        1,
        true,
        "thinking".into(),
    ));
    observed.turn_id = turn_id.map(str::to_owned);
    observed
}

fn user_observation() -> AdapterObservation {
    // Native user echoes carry no prompt id in the ACP frame.
    AdapterObservation::structured(crate::adapters::message_payload(
        Id::new("obj").unwrap(),
        MessageRole::User,
        "go".into(),
    ))
}

fn assistant_observation(turn_id: Option<&str>) -> AdapterObservation {
    let mut observed = AdapterObservation::structured(crate::adapters::message_chunk(
        Id::new("obj").unwrap(),
        1,
        true,
        "answer".into(),
    ));
    observed.turn_id = turn_id.map(str::to_owned);
    observed
}

fn tool_call_observation(
    call_id: &str,
    name: &str,
    input: Option<Value>,
    state: ToolCallState,
    turn_id: Option<&str>,
) -> AdapterObservation {
    let mut observed = AdapterObservation::structured(crate::adapters::tool_call_payload(
        Id::new("obj").unwrap(),
        Some(name.to_owned()),
        input,
    ));
    if let ObservationPayload::ToolCall(call) = &mut observed.payload {
        call.state = state;
    }
    observed.item_id = Some(call_id.to_owned());
    observed.turn_id = turn_id.map(str::to_owned);
    observed
}

fn tool_result_observation(
    call_id: &str,
    update: Value,
    turn_id: Option<&str>,
) -> AdapterObservation {
    let mut observed = AdapterObservation::structured(crate::adapters::tool_result_payload(
        Id::new("obj").unwrap(),
        None,
        Some(update),
        None,
        ToolOutcome::Succeeded,
    ));
    observed.item_id = Some(call_id.to_owned());
    observed.turn_id = turn_id.map(str::to_owned);
    observed
}

fn question_input() -> Value {
    // Synthesized from docs, not captured [U]: the request shape mirrors the
    // fixture's ask_user_question Pending frame.
    json!({
        "questions": [{
            "question": "Choose one.",
            "options": [
                {"label": "Alpha", "description": "a"},
                {"label": "Beta", "description": "b"}
            ]
        }]
    })
}

fn just_phases(out: &[AdapterObservation]) -> Vec<(String, String)> {
    phase_events(out)
        .into_iter()
        .map(|(name, phase, _)| (name, phase))
        .collect()
}

#[test]
fn one_phase_lifecycle_per_episode_not_per_chunk() {
    // Synthesized from docs, not captured [U]. Live ordering: boundary first.
    let mut fold = LiveProjection::default();
    let input = vec![
        turn_observation("turn_started", None),
        thought_observation(Some("prompt-1")),
        thought_observation(Some("prompt-1")),
        tool_call_observation(
            "call-1",
            "run_terminal_command",
            Some(json!({"command": "true"})),
            ToolCallState::Proposed,
            Some("prompt-1"),
        ),
        tool_call_observation(
            "call-1",
            "run_terminal_command",
            Some(json!({"command": "true"})),
            ToolCallState::Running,
            Some("prompt-1"),
        ),
        tool_call_observation(
            "call-1",
            "run_terminal_command",
            Some(json!({"command": "true"})),
            ToolCallState::Running,
            Some("prompt-1"),
        ),
        turn_observation("turn_ended", Some("completed")),
    ];
    let out = fold.fold_batch(input, &identity(), fixed_now());
    assert_eq!(
        just_phases(&out),
        vec![
            ("turn_started".into(), "prompt-accepted".into()),
            ("turn.live".into(), "thinking".into()),
            ("turn.live".into(), "tool-started".into()),
            ("turn.live".into(), "tool-output".into()),
            ("turn_ended".into(), "turn-ended".into()),
        ]
    );
}

#[test]
fn thinking_reopens_after_a_tool_in_the_same_turn() {
    // Synthesized from docs, not captured [U] (defect 1): think, tool, think.
    let mut fold = LiveProjection::default();
    let completed = json!({"rawOutput": {}});
    let input = vec![
        turn_observation("turn_started", None),
        thought_observation(Some("p1")),
        tool_call_observation(
            "call-1",
            "run_terminal_command",
            None,
            ToolCallState::Proposed,
            Some("p1"),
        ),
        tool_result_observation("call-1", completed, Some("p1")),
        thought_observation(Some("p1")),
        assistant_observation(Some("p1")),
        turn_observation("turn_ended", Some("completed")),
    ];
    let out = fold.fold_batch(input, &identity(), fixed_now());
    let phases: Vec<_> = just_phases(&out).into_iter().map(|(_, p)| p).collect();
    assert_eq!(
        phases,
        vec![
            "prompt-accepted",
            "thinking",
            "tool-started",
            "tool-finished",
            "thinking",
            "text-streaming",
            "turn-ended",
        ]
    );
}

#[test]
fn two_turns_replayed_in_one_batch_each_get_their_stream_phases_and_prompt_ids() {
    // Synthesized from docs, not captured [U] (defect 2): the first poll after
    // attach replays both turns' updates before any boundary.
    let mut fold = LiveProjection::default();
    let input = vec![
        user_observation(),
        thought_observation(Some("p1")),
        assistant_observation(Some("p1")),
        user_observation(),
        thought_observation(Some("p2")),
        assistant_observation(Some("p2")),
        turn_observation("turn_started", None),
        turn_observation("turn_ended", Some("completed")),
        turn_observation("turn_started", None),
        turn_observation("turn_ended", Some("completed")),
    ];
    let out = fold.fold_batch(input, &identity(), fixed_now());
    let phases = phase_events(&out);
    let by = |prompt: &str| {
        phases
            .iter()
            .filter(|(_, _, id)| id.as_deref() == Some(prompt))
            .map(|(_, phase, _)| phase.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(by("p1"), vec!["thinking", "text-streaming", "turn-ended"]);
    assert_eq!(by("p2"), vec!["thinking", "text-streaming", "turn-ended"]);
    // The anchors ride synthesized lifecycles (user chunks arrived first); the
    // two turn_started boundaries are untagged.
    assert_eq!(
        phases
            .iter()
            .filter(|(name, phase, _)| name == "turn.live" && phase == "prompt-accepted")
            .count(),
        2
    );
    assert!(!phases.iter().any(|(name, _, _)| name == "turn_started"));
}

#[test]
fn live_ordering_tags_the_boundary_and_does_not_double_anchor() {
    // Synthesized from docs, not captured [U]: the events file lands first, so
    // prompt-accepted rides turn_started and the later user echo anchors
    // nothing.
    let mut fold = LiveProjection::default();
    let input = vec![
        turn_observation("turn_started", None),
        user_observation(),
        thought_observation(Some("p1")),
        turn_observation("turn_ended", Some("completed")),
    ];
    let out = fold.fold_batch(input, &identity(), fixed_now());
    assert_eq!(
        just_phases(&out),
        vec![
            ("turn_started".into(), "prompt-accepted".into()),
            ("turn.live".into(), "thinking".into()),
            ("turn_ended".into(), "turn-ended".into()),
        ]
    );
}

#[test]
fn a_pending_question_is_resolved_native_cleared_at_turn_end() {
    // Synthesized from docs, not captured [U]: question requested but the turn
    // ends without a completed frame.
    let mut fold = LiveProjection::default();
    let input = vec![
        turn_observation("turn_started", None),
        tool_call_observation(
            "call-q",
            "ask_user_question",
            Some(question_input()),
            ToolCallState::Proposed,
            Some("prompt-1"),
        ),
        turn_observation("turn_ended", Some("completed")),
    ];
    let out = fold.fold_batch(input, &identity(), fixed_now());
    let requested = requested_interactions(&out);
    assert_eq!(requested.len(), 1);
    assert!(!requested[0].answerable);
    let resolved = resolved_entities(&out);
    assert_eq!(
        resolved.len(),
        1,
        "the turn-end sweep resolves the question"
    );
    assert_eq!(resolved[0].meta.id, requested[0].meta.id);
    assert_eq!(resolved[0].meta.revision.0, 2);
    assert_eq!(resolved[0].state, InteractionState::Resolved);
    let reason = match &resolved[0].resolution {
        Knowledge::Known { value } => &value.reason,
        _ => panic!("resolution"),
    };
    assert_eq!(*reason, InteractionResolutionReason::NativeCleared);
    assert!(matches!(resolved[0].answer, Knowledge::Unknown { .. }));
}

#[test]
fn an_answer_that_matches_no_offered_label_resolves_native_cleared() {
    // Synthesized from docs, not captured [U]: UserAnswered names an option the
    // request never offered.
    let mut fold = LiveProjection::default();
    let completed = json!({
        "rawOutput": {
            "UserAnswered": {
                "message": "User has answered your questions: \"Choose one.\"=\"Gamma\". Done."
            }
        }
    });
    let input = vec![
        turn_observation("turn_started", None),
        tool_call_observation(
            "call-q",
            "ask_user_question",
            Some(question_input()),
            ToolCallState::Proposed,
            Some("prompt-1"),
        ),
        tool_result_observation("call-q", completed, Some("prompt-1")),
        turn_observation("turn_ended", Some("completed")),
    ];
    let out = fold.fold_batch(input, &identity(), fixed_now());
    let resolved = resolved_entities(&out);
    assert_eq!(resolved.len(), 1);
    let reason = match &resolved[0].resolution {
        Knowledge::Known { value } => &value.reason,
        _ => panic!("resolution"),
    };
    assert_eq!(*reason, InteractionResolutionReason::NativeCleared);
    assert!(matches!(resolved[0].answer, Knowledge::Unknown { .. }));
}

#[test]
fn a_structured_answers_object_commits_the_matching_label() {
    // Synthesized from docs, not captured [U]: a future client writes a
    // structured `answers` object keyed by question text.
    let mut fold = LiveProjection::default();
    let completed = json!({
        "rawOutput": {"UserAnswered": {"answers": {"Choose one.": "Beta"}}}
    });
    let input = vec![
        turn_observation("turn_started", None),
        tool_call_observation(
            "call-q",
            "ask_user_question",
            Some(question_input()),
            ToolCallState::Proposed,
            Some("prompt-1"),
        ),
        tool_result_observation("call-q", completed, Some("prompt-1")),
        turn_observation("turn_ended", Some("completed")),
    ];
    let out = fold.fold_batch(input, &identity(), fixed_now());
    let resolved = resolved_entities(&out);
    assert_eq!(resolved.len(), 1);
    let reason = match &resolved[0].resolution {
        Knowledge::Known { value } => &value.reason,
        _ => panic!("resolution"),
    };
    assert_eq!(*reason, InteractionResolutionReason::Answered);
    let answer = match &resolved[0].answer {
        Knowledge::Known { value } => value,
        _ => panic!("answer"),
    };
    let remuda_protocol::InteractionAnswer::Question(question) = &answer.value else {
        panic!("question answer");
    };
    assert_eq!(
        question.answers.get("q0").unwrap().option_ids,
        vec!["Beta".to_owned()]
    );
}

#[test]
fn a_multi_select_question_builds_a_multi_field() {
    // Synthesized from docs, not captured [U].
    let mut fold = LiveProjection::default();
    let input = json!({
        "questions": [{
            "question": "Pick several.",
            "multiSelect": true,
            "options": [{"label": "Alpha"}, {"label": "Beta"}]
        }]
    });
    let out = fold.fold_batch(
        vec![
            turn_observation("turn_started", None),
            tool_call_observation(
                "call-q",
                "ask_user_question",
                Some(input),
                ToolCallState::Proposed,
                Some("prompt-1"),
            ),
        ],
        &identity(),
        fixed_now(),
    );
    let requested = requested_interactions(&out);
    assert_eq!(requested.len(), 1);
    let InteractionRequest::Question(request) = &requested[0].request else {
        panic!("question");
    };
    assert_eq!(request.fields[0].input, QuestionInput::MultiSelect);
}

#[test]
fn a_promptless_tool_call_is_the_first_frame_without_panicking() {
    // Synthesized from docs, not captured [U]: a tool_call / tool_call_update
    // pair whose frames carry no _meta.promptId (the adapter leaves turn_id
    // None) is the very first content a fresh fold sees. The fold must open a
    // slot rather than index an empty vec.
    let mut fold = LiveProjection::default();
    let out = fold.fold_batch(
        vec![
            tool_call_observation(
                "call-x",
                "run_terminal_command",
                Some(json!({"command": "true"})),
                ToolCallState::Proposed,
                None,
            ),
            tool_result_observation("call-x", json!({"rawOutput": {}}), None),
        ],
        &identity(),
        fixed_now(),
    );
    let phases = just_phases(&out);
    assert_eq!(
        phases,
        vec![
            ("turn.live".into(), "tool-started".into()),
            ("turn.live".into(), "tool-finished".into()),
        ]
    );
}

#[test]
fn finished_slots_are_pruned_and_a_pruned_turn_reopens_nothing() {
    // Synthesized from docs, not captured [U]: eleven complete turns in live
    // ordering, then a stray thought for the pruned first turn.
    let mut fold = LiveProjection::default();
    let mut input = Vec::new();
    for n in 0..11 {
        let prompt = format!("p{n}");
        input.push(turn_observation("turn_started", None));
        input.push(thought_observation(Some(&prompt)));
        input.push(turn_observation("turn_ended", Some("completed")));
    }
    let mut out = fold.fold_batch(input, &identity(), fixed_now());
    assert_eq!(fold.live_slot_count(), RETAINED_SLOTS);
    let count = |phase: &str| just_phases(&out).iter().filter(|(_, p)| p == phase).count();
    assert_eq!(count("prompt-accepted"), 11);
    assert_eq!(count("thinking"), 11);
    assert_eq!(count("turn-ended"), 11);
    // The last turn's boundary is tagged with its own prompt id.
    let tagged = phase_events(&out);
    assert!(
        tagged
            .iter()
            .any(|(_, phase, prompt)| phase == "turn-ended" && prompt.as_deref() == Some("p10"))
    );
    // A late streaming frame for the pruned first turn journals as a fact but
    // opens no new live evidence.
    let extra = fold.fold_batch(
        vec![thought_observation(Some("p0"))],
        &identity(),
        fixed_now(),
    );
    out.extend(extra);
    assert_eq!(
        just_phases(&out)
            .iter()
            .filter(|(_, p)| p == "thinking")
            .count(),
        11,
        "pruned turn cannot re-open a phase"
    );
}
