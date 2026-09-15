//! Task/subagent row lifecycle through the claude-pty transcript mapper:
//! a synchronous foreground Agent closes Final; a backgrounded launch returns
//! an immediate Partial result and closes Final on its `<task-notification>`;
//! a killed launch closes Failed.
//!
//! Fixture `claude-transcript-tasktrack.jsonl` is a sanitized slice of real
//! 2.1.221/2.1.272 transcripts (the fg/bg pair was captured live for
//! c-tasktrack on 2026-09-15). Before the fix every Agent launch — even the
//! background one whose subagent was still running — mapped to a Final result,
//! and the later notification was an unrelated user message, so the task row
//! never reflected the real completion.

use remuda_driver::{DriverKind, TranscriptMapper};
use remuda_protocol::{
    ContentBlock, HostId, Id, InstanceId, MutationOperation, ObservationPayload, ResultStage,
    RunId, ToolCategory, ToolOutcome,
};
use std::path::Path;

fn derived(instance: &InstanceId, native_tool_id: &str) -> Id {
    Id::derive("obj", instance.as_id().as_str(), native_tool_id).expect("derive tool id")
}

fn fixture() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude-transcript-tasktrack.jsonl"),
    )
    .expect("fixture")
}

fn hydrate() -> (InstanceId, Vec<remuda_protocol::Observation>) {
    let body = fixture();
    let instance = InstanceId::new();
    let mut mapper = TranscriptMapper::new(
        DriverKind::ClaudePty,
        instance.clone(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "55555555-6666-4777-8888-999999999999".into(),
        "2.1.221".into(),
    );
    let mut out: Vec<_> = body
        .lines()
        .flat_map(|line| mapper.map_line(line).expect("map transcript line"))
        .collect();
    out.extend(mapper.flush().expect("flush"));
    (instance, out)
}

fn result_text(result: &remuda_protocol::ToolResultPayload) -> String {
    result
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn derived_result(instance: &InstanceId, native_tool_id: &str) -> Id {
    // The result occupies its own node keyed `tool-result:{native}`.
    Id::derive(
        "obj",
        instance.as_id().as_str(),
        &format!("tool-result:{native_tool_id}"),
    )
    .expect("derive result id")
}

fn results_for<'a>(
    instance: &InstanceId,
    observations: &'a [remuda_protocol::Observation],
    native_tool_id: &str,
) -> Vec<&'a remuda_protocol::ToolResultPayload> {
    let target = derived_result(instance, native_tool_id);
    observations
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::ToolResult(result) if result.mutation.node_id == target => {
                Some(result.as_ref())
            }
            _ => None,
        })
        .collect()
}

fn call_for<'a>(
    instance: &InstanceId,
    observations: &'a [remuda_protocol::Observation],
    native_tool_id: &str,
) -> Option<&'a remuda_protocol::ToolCallPayload> {
    let target = derived(instance, native_tool_id);
    observations.iter().find_map(|obs| match &obs.body {
        ObservationPayload::ToolCall(call) if call.tool_call_id == target => Some(call.as_ref()),
        _ => None,
    })
}

#[test]
fn a_synchronous_foreground_agent_closes_final_with_one_result() {
    let (instance, observations) = hydrate();
    let call =
        call_for(&instance, &observations, "toolu_recorded_task_fg_sync").expect("agent call");
    assert_eq!(call.category, ToolCategory::Agent);
    let results = results_for(&instance, &observations, "toolu_recorded_task_fg_sync");
    assert_eq!(results.len(), 1, "a sync foreground agent has one result");
    assert_eq!(results[0].stage, ResultStage::Final);
    assert_eq!(results[0].outcome, ToolOutcome::Succeeded);
}

#[test]
fn a_background_launch_is_partial_then_final_on_its_notification() {
    let (instance, observations) = hydrate();
    let results = results_for(&instance, &observations, "toolu_recorded_task_bg");
    assert_eq!(results.len(), 2, "launch result then completion result");
    let (launch, completion) = (results[0], results[1]);
    assert_eq!(launch.stage, ResultStage::Partial, "launch is not final");
    assert_eq!(launch.outcome, ToolOutcome::Succeeded);
    assert_eq!(completion.stage, ResultStage::Final);
    assert_eq!(completion.outcome, ToolOutcome::Succeeded);
    assert_eq!(result_text(completion), "DONE");
    // Monotonic revisions: the completion must overwrite the launch.
    assert!(
        completion.mutation.revision.0 > launch.mutation.revision.0,
        "completion rev {} > launch rev {}",
        completion.mutation.revision.0,
        launch.mutation.revision.0
    );
    // The result is its own node (open→replace) but joins the call via
    // `tool_call_id`, matching the live stream's mutation sequence.
    let call = call_for(&instance, &observations, "toolu_recorded_task_bg").unwrap();
    assert_ne!(call.tool_call_id, launch.mutation.node_id);
    assert_eq!(call.tool_call_id, launch.tool_call_id);
    assert_eq!(call.tool_call_id, completion.tool_call_id);
    assert_eq!(launch.mutation.operation, MutationOperation::Open);
    assert_eq!(completion.mutation.operation, MutationOperation::Replace);
}

#[test]
fn a_killed_background_agent_closes_failed() {
    let (instance, observations) = hydrate();
    let results = results_for(&instance, &observations, "toolu_recorded_task_killed");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].stage, ResultStage::Partial);
    assert_eq!(results[1].stage, ResultStage::Final);
    assert_eq!(
        results[1].outcome,
        ToolOutcome::Failed,
        "<status>killed</status> must read as failed"
    );
}
