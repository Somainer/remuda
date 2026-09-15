//! File-transcript mapper fold for subagent task rows (c-tasktrack).
//!
//! Same fixture/contract as the driver mapper test, covering the
//! `remuda-journal::claude` mapper used by file/replay channels: a sync
//! foreground Agent closes Final; an async launch returns Partial and closes
//! Final (Failed when killed) on its injected `<task-notification>`.

use remuda_journal::{MapContext, NativeIds, digest_of, map_claude_line};
use remuda_protocol::{
    FileCursor, HostId, Id, InstanceId, ObservationPayload, ResultStage, SourceChannel,
    ToolOutcome, U64,
};

const FIXTURE: &str = include_str!("fixtures/claude-transcript-tasktrack.jsonl");

fn map_with(instance: &InstanceId) -> Vec<ObservationPayload> {
    let ctx = MapContext::claude_file(
        instance.clone(),
        Id::new("obj").unwrap(),
        HostId::new(),
        "55555555-6666-4777-8888-999999999999",
        SourceChannel::Transcript,
    );
    let mut ids = NativeIds::new(instance.as_id().as_str());
    let mut envelopes = Vec::new();
    let mut offset = 0u64;
    for line in FIXTURE.lines().filter(|l| !l.is_empty()) {
        let cursor = FileCursor {
            file_identity: Id::new("obj").unwrap(),
            file_generation: U64(1),
            offset: U64(offset),
            length: U64(line.len() as u64),
            digest: digest_of(line.as_bytes()),
        };
        offset += line.len() as u64 + 1;
        let mapped = map_claude_line(&ctx, &mut ids, line.as_bytes(), &cursor).unwrap();
        envelopes.extend(mapped);
    }
    envelopes
        .into_iter()
        .map(|envelope| envelope.body)
        .collect()
}

fn derived(instance: &InstanceId, native: &str) -> Id {
    Id::derive("obj", instance.as_id().as_str(), native).unwrap()
}

fn results<'a>(
    payloads: &'a [ObservationPayload],
    target: &Id,
) -> Vec<&'a remuda_protocol::ToolResultPayload> {
    payloads
        .iter()
        .filter_map(|payload| match payload {
            ObservationPayload::ToolResult(result) if result.tool_call_id == *target => {
                Some(result.as_ref())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn file_mapper_folds_foreground_background_and_killed_subagents() {
    let instance = InstanceId::new();
    let payloads = map_with(&instance);

    let fg = results(
        &payloads,
        &derived(&instance, "toolu_recorded_task_fg_sync"),
    );
    assert_eq!(fg.len(), 1);
    assert_eq!(fg[0].stage, ResultStage::Final);
    assert_eq!(fg[0].outcome, ToolOutcome::Succeeded);

    let bg = results(&payloads, &derived(&instance, "toolu_recorded_task_bg"));
    assert_eq!(bg.len(), 2, "launch then completion");
    assert_eq!(bg[0].stage, ResultStage::Partial);
    assert_eq!(bg[1].stage, ResultStage::Final);
    assert_eq!(bg[1].outcome, ToolOutcome::Succeeded);
    assert!(bg[1].mutation.revision.0 > bg[0].mutation.revision.0);

    let killed = results(&payloads, &derived(&instance, "toolu_recorded_task_killed"));
    assert_eq!(killed.len(), 2);
    assert_eq!(killed[0].stage, ResultStage::Partial);
    assert_eq!(killed[1].stage, ResultStage::Final);
    assert_eq!(killed[1].outcome, ToolOutcome::Failed);
}
