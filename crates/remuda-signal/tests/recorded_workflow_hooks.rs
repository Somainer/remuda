//! Replay of workflow hook payloads recorded from `claude` 2.1.221 (r-ux-w).
//!
//! Companion to `recorded_hooks.rs` (2.1.270): this is the evidence for the
//! timeline-card spike. It pins the facts the Node fold and the card degrade
//! path rely on — SubagentStart/Stop carry the agent id, sub-agent tool hooks
//! carry it too, the launch response is structured, and label/phase/model are
//! simply absent on this build.

use remuda_protocol::{LifecyclePayload, ObservationPayload};
use remuda_signal::{HookEnvelope, HookEvent, map_event};

fn recorded() -> Vec<HookEvent> {
    remuda_testing::hook_workflow_fixture()
        .lines()
        .filter(|line: &&str| !line.trim().is_empty())
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

fn related_of(event: &HookEvent) -> std::collections::BTreeMap<String, String> {
    let mapped = map_event(event);
    let ObservationPayload::Lifecycle(lifecycle) = &mapped.payload else {
        panic!("expected native lifecycle");
    };
    let LifecyclePayload::Native(n) = lifecycle.as_ref() else {
        panic!("expected native lifecycle");
    };
    n.related_ids.clone()
}

#[test]
fn the_recording_covers_the_workflow_event_vocabulary() {
    let names: Vec<String> = recorded().iter().map(|e| e.name.clone()).collect();
    for required in [
        "SubagentStart",
        "SubagentStop",
        "PreToolUse",
        "PostToolUse",
        "PostToolBatch",
    ] {
        assert!(names.iter().any(|n| n == required), "missing {required}");
    }
}

#[test]
fn the_workflow_launch_response_is_structured_with_runid_and_paths() {
    let events = recorded();
    let launch = events
        .iter()
        .find(|e| {
            e.name == "PostToolUse"
                && e.payload
                    .get("tool_response")
                    .and_then(|r| r.get("runId"))
                    .is_some()
        })
        .expect("a structured Workflow PostToolUse");
    let related = related_of(launch);
    for key in [
        "runId",
        "taskId",
        "workflowName",
        "transcriptDir",
        "scriptPath",
    ] {
        assert!(
            related.contains_key(key),
            "launch must curate {key}; got {related:?}"
        );
    }
    assert_eq!(related["runId"], "wf_c3422384-cb1");
    assert!(
        related["transcriptDir"].ends_with("subagents/workflows/wf_c3422384-cb1"),
        "{}",
        related["transcriptDir"]
    );
}

#[test]
fn subagent_lifecycle_and_tool_hooks_carry_the_agent_id() {
    let events = recorded();
    let start = events
        .iter()
        .find(|e| e.name == "SubagentStart")
        .expect("SubagentStart");
    let start_related = related_of(start);
    assert_eq!(start_related["agentType"], "workflow-subagent");
    assert!(start_related["agentId"].len() >= 12);

    // A sub-agent tool hook names the same agent, which is how the fold joins
    // live tool activity to its member row.
    let sub_tool = events
        .iter()
        .find(|e| e.name == "PostToolUse" && e.payload.get("agent_id").is_some())
        .expect("sub-agent PostToolUse");
    let tool_related = related_of(sub_tool);
    assert_eq!(tool_related["toolName"], "Bash");
    assert_eq!(tool_related["agentId"], "aae139d44933cefe2");
    assert_eq!(tool_related["durationMs"], "4303");
}

#[test]
fn subagent_stop_names_its_transcript_and_the_background_workflow() {
    let events = recorded();
    let stop = events
        .iter()
        .find(|e| {
            e.name == "SubagentStop"
                && e.payload
                    .get("agent_transcript_path")
                    .and_then(|v| v.as_str())
                    .is_some_and(|p| p.contains("wf_c3422384-cb1"))
        })
        .expect("SubagentStop with transcript path");
    let related = related_of(stop);
    assert!(
        related["agentTranscriptPath"].ends_with("agent-aae139d44933cefe2.jsonl"),
        "{}",
        related["agentTranscriptPath"]
    );
    assert_eq!(related["backgroundTaskId"], "wn1eumvco");
    assert_eq!(related["backgroundTaskStatus"], "running");
    assert_eq!(related["backgroundTaskName"], "canary-wf");
}

#[test]
fn label_and_phase_are_absent_everywhere_on_221() {
    // The card must degrade rather than invent these; 2.1.270+ writes them to
    // agent meta.json, this build does not.
    for event in recorded() {
        assert!(event.payload.get("label").is_none(), "{:?}", event.name);
        assert!(event.payload.get("phase").is_none(), "{:?}", event.name);
    }
}
