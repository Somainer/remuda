//! Live-layer fold for subagent task rows (c-tasktrack).
//!
//! A backgrounded `Agent`/`Task` launch returns an immediate
//! `async_launched` response — the subagent is still running. Its
//! PostToolUse result must be `Partial` (the task track reads "running in
//! background"), and a later `SubagentStop` carrying the same `agent_id`
//! closes that exact node with a `Final` result.
//!
//! `SubagentStop` stays non-turn-evidence: closing a task row is a tool
//! mutation, never a turn transition (no phase, raw lifecycle untouched).

use remuda_protocol::{ObservationPayload, ResultStage, ToolOutcome};
use remuda_signal::{HookEvent, LiveState, map_event};

const SCOPE: &str = "ins_test_scope";
const NOW: &str = "2026-09-15T00:00:00.000Z";
const TOOL: &str = "toolu_vrtx_bg1";
const AGENT: &str = "acf1e742ec621d5f2";

fn event(name: &str, payload: serde_json::Value) -> HookEvent {
    HookEvent {
        name: name.into(),
        ppid: 4242,
        payload,
    }
}

fn results(extras: &[ObservationPayload]) -> Vec<&remuda_protocol::ToolResultPayload> {
    extras
        .iter()
        .filter_map(|payload| match payload {
            ObservationPayload::ToolResult(result) => Some(result.as_ref()),
            _ => None,
        })
        .collect()
}

/// A full background lifecycle: Pre → Post(async launch) → SubagentStop.
fn fold_background() -> (LiveState, Vec<ObservationPayload>, Vec<String>) {
    let events = [
        event(
            "PreToolUse",
            serde_json::json!({"tool_use_id":TOOL,"tool_name":"Agent","tool_input":{"run_in_background":true}}),
        ),
        event(
            "PostToolUse",
            serde_json::json!({
                "tool_use_id": TOOL,
                "tool_name": "Agent",
                "tool_response": {"isAsync": true, "status": "async_launched", "agentId": AGENT},
            }),
        ),
        event(
            "SubagentStop",
            serde_json::json!({
                "agent_id": AGENT,
                "agent_type": "general-purpose",
                "last_assistant_message": "DONE",
                "background_tasks": [],
            }),
        ),
    ];
    let mut state = LiveState::new(SCOPE);
    let mut extras = Vec::new();
    let mut phases = Vec::new();
    for event in events {
        let mapped = map_event(&event);
        let fold = state.observe(&event, &mapped, NOW, true);
        extras.extend(fold.extras);
        if let Some(phase) = fold.related.get("phase") {
            phases.push(phase.clone());
        }
    }
    (state, extras, phases)
}

#[test]
fn a_background_launch_is_partial_and_subagent_stop_closes_it_final() {
    let (_state, extras, _phases) = fold_background();
    let results = results(&extras);
    assert_eq!(results.len(), 2, "launch result then completion result");
    assert_eq!(results[0].stage, ResultStage::Partial, "launch is partial");
    assert_eq!(results[1].stage, ResultStage::Final, "completion is final");
    assert_eq!(results[1].outcome, ToolOutcome::Succeeded);
    // Both land on the one launch node.
    assert_eq!(results[0].tool_call_id, results[1].tool_call_id);
    // Call 1–2, launch result 3, completion 4.
    assert_eq!(results[0].mutation.revision.0, 3);
    assert_eq!(results[1].mutation.revision.0, 4);
}

#[test]
fn subagent_stop_emits_no_phase_it_is_not_turn_evidence() {
    let (_state, _extras, phases) = fold_background();
    // The PreToolUse/PostToolUse pair carries tool-started/tool-finished;
    // SubagentStop must contribute no phase tag.
    assert!(
        !phases.is_empty(),
        "the tool events still carry their phases"
    );
    // Independently: a lone SubagentStop never latches a phase nor emits a
    // raw lifecycle turn tag.
    let mut state = LiveState::new(SCOPE);
    let ev = event(
        "SubagentStop",
        serde_json::json!({"agent_id": AGENT, "background_tasks": []}),
    );
    let mapped = map_event(&ev);
    let fold = state.observe(&ev, &mapped, NOW, true);
    assert!(
        !fold.related.contains_key("phase"),
        "no phase from SubagentStop"
    );
    assert!(fold.extras.is_empty(), "unknown agent closes nothing");
    assert!(!fold.transition);
}

#[test]
fn subagent_stop_for_an_unmatched_agent_closes_nothing() {
    // A SubagentStop with no tracked agent (a foreground subagent already
    // closed, or the no-subagent recap case) emits no tool result.
    let mut state = LiveState::new(SCOPE);
    let pre = event(
        "PreToolUse",
        serde_json::json!({"tool_use_id":TOOL,"tool_name":"Agent","tool_input":{}}),
    );
    let mapped = map_event(&pre);
    state.observe(&pre, &mapped, NOW, true);
    let stop = event(
        "SubagentStop",
        serde_json::json!({"agent_id": "some-other-id"}),
    );
    let mapped = map_event(&stop);
    let fold = state.observe(&stop, &mapped, NOW, true);
    assert!(fold.extras.is_empty(), "no row to close");
    assert!(!fold.related.contains_key("phase"));
}

#[test]
fn a_foreground_agent_post_tool_use_stays_final_and_needs_no_stop() {
    // Foreground Agent: PostToolUse carries the real completed result (no
    // isAsync), so the result is Final at revision 2 as before.
    let mut state = LiveState::new(SCOPE);
    let mut extras = Vec::new();
    for (name, payload) in [
        (
            "PreToolUse",
            serde_json::json!({"tool_use_id":TOOL,"tool_name":"Agent","tool_input":{}}),
        ),
        (
            "PostToolUse",
            serde_json::json!({"tool_use_id":TOOL,"tool_name":"Agent","tool_response":{"stdout":"report"}}),
        ),
    ] {
        let ev = event(name, payload);
        let mapped = map_event(&ev);
        extras.extend(state.observe(&ev, &mapped, NOW, true).extras);
    }
    let results = results(&extras);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].stage, ResultStage::Final);
    assert_eq!(results[0].outcome, ToolOutcome::Succeeded);
    assert_eq!(results[0].mutation.revision.0, 2);
}
