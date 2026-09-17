//! §9.1 transcript model read-back: `model` observations, slash/verdict
//! detection, and dedup — driven through the real [`TranscriptMapper`] with a
//! verbatim replay of a real claude 2.1.272 PTY session
//! (`fixtures/model-21272/`, measured by `examples/model_probe.rs`, evidence
//! model-sync-1.md).

use remuda_driver::TranscriptMapper;
use remuda_protocol::{
    DriverKind, EffortSource, HostId, Id, InstanceId, ObservationPayload, RunId,
};
use serde_json::json;

fn mapper() -> TranscriptMapper {
    TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "model-session".into(),
        "2.1.272".into(),
    )
}

fn assistant(model: Option<&str>, n: u64) -> String {
    json!({
        "type": "assistant",
        "uuid": format!("msg-{n}"),
        "sessionId": "model-session",
        "message": {
            "id": format!("msg-{n}"),
            "role": "assistant",
            "type": "message",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "model": model,
        },
    })
    .to_string()
}

fn user_slash(id: &str, n: u64) -> String {
    json!({
        "type": "user",
        "uuid": format!("cmd-{n}"),
        "sessionId": "model-session",
        "message": {
            "role": "user",
            "content": format!(
                "<command-name>/model</command-name>\n<command-message>model</command-message>\n\
                 <command-args>{id}</command-args>"
            )
        }
    })
    .to_string()
}

fn user_stdout(line: &str, n: u64) -> String {
    json!({
        "type": "user",
        "uuid": format!("out-{n}"),
        "sessionId": "model-session",
        "message": {
            "role": "user",
            "content": format!("<local-command-stdout>{line}</local-command-stdout>")
        }
    })
    .to_string()
}

fn system_record(content: &str, n: u64, with_run: bool) -> String {
    let mut v = json!({
        "type": "system",
        "subtype": "local_command",
        "uuid": format!("sys-{n}"),
        "sessionId": "model-session",
        "level": "info",
        "content": content,
    });
    if with_run {
        v["commandRun"] = json!({"command": "model", "args": "bogus-xyz-123"});
    }
    v.to_string()
}

fn model_edges(out: &[remuda_protocol::Observation]) -> Vec<(String, EffortSource)> {
    out.iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::Model(payload) => {
                Some((payload.effective.id.clone(), payload.effective.source))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn first_assistant_model_emits_then_identical_dedupes() {
    let mut mapper = mapper();
    let edges = model_edges(
        &mapper
            .map_line(&assistant(Some("claude-opus-5"), 1))
            .unwrap(),
    );
    assert_eq!(
        edges,
        vec![("claude-opus-5".to_string(), EffortSource::Unknown)]
    );
    assert!(
        model_edges(
            &mapper
                .map_line(&assistant(Some("claude-opus-5"), 2))
                .unwrap()
        )
        .is_empty()
    );
}

#[test]
fn accepted_switch_settles_from_the_stdout_verdict_without_an_assistant_record() {
    let mut mapper = mapper();
    mapper.map_line(&assistant(Some("a"), 0)).unwrap();
    mapper.map_line(&user_slash("model_hub/x", 1)).unwrap();
    let out = mapper
        .map_line(&user_stdout(
            "Set model to `model_hub/x` and saved as your default for new sessions",
            1,
        ))
        .unwrap();
    assert_eq!(
        model_edges(&out),
        vec![("model_hub/x".to_string(), EffortSource::Slash)]
    );
    // A later assistant record with that id is deduped.
    assert!(model_edges(&mapper.map_line(&assistant(Some("model_hub/x"), 2)).unwrap()).is_empty());
}

#[test]
fn alias_resolution_reports_the_resolved_id_not_the_typed_word() {
    let mut mapper = mapper();
    mapper
        .map_line(&assistant(Some("model_hub/es1_orange_o48[1m]"), 0))
        .unwrap();
    mapper.map_line(&user_slash("sonnet", 1)).unwrap();
    // The verdict spells the concrete gateway id the alias resolved to.
    let out = mapper
        .map_line(&user_stdout(
            "Set model to `model_hub/es1_orange_o48[1m]` and saved as your default for new sessions",
            1,
        ))
        .unwrap();
    // Same concrete id: no edge (the picker selection settles from read-back).
    assert!(model_edges(&out).is_empty());
}

#[test]
fn env_hint_second_line_does_not_poison_the_resolved_id() {
    let mut mapper = mapper();
    mapper
        .map_line(&user_slash("model_hub/es1_orange_o50", 1))
        .unwrap();
    let out = mapper
        .map_line(&user_stdout(
            "Set model to `model_hub/es1_orange_o50` and saved as your default for new sessions\n\
                 ANTHROPIC_MODEL is set to `model_hub/es1_orange_o48[1m]`",
            1,
        ))
        .unwrap();
    assert_eq!(
        model_edges(&out),
        vec![("model_hub/es1_orange_o50".to_string(), EffortSource::Slash)]
    );
}

#[test]
fn bogus_and_kept_system_records_emit_no_model_edge() {
    let mut mapper = mapper();
    mapper.map_line(&assistant(Some("a"), 0)).unwrap();
    // /model bogus → system pair: not found.
    mapper
        .map_line(&system_record(
            "<command-name>/model</command-name>\n<command-message>model</command-message>\n\
             <command-args>bogus-xyz-123</command-args>",
            1,
            false,
        ))
        .unwrap();
    mapper
        .map_line(&system_record(
            "<local-command-stdout>Model 'bogus-xyz-123' not found</local-command-stdout>",
            2,
            true,
        ))
        .unwrap();
    // Bare /model dismissed → system pair: kept.
    mapper
        .map_line(&system_record(
            "<command-name>/model</command-name>\n<command-args></command-args>",
            3,
            false,
        ))
        .unwrap();
    mapper
        .map_line(&system_record(
            "<local-command-stdout>Kept model as `a`</local-command-stdout>",
            4,
            true,
        ))
        .unwrap();
    // A same-id assistant record afterwards is still deduped, not an edge.
    assert!(model_edges(&mapper.map_line(&assistant(Some("a"), 9)).unwrap()).is_empty());
}

// ───────── Real claude 2.1.272 PTY session, replayed verbatim ───────────────

const WALK: &str = include_str!("fixtures/model-21272/model-walk-21272.jsonl");
const REJECT: &str = include_str!("fixtures/model-21272/model-reject-21272.jsonl");

fn replay(path: &str) -> Vec<(String, EffortSource)> {
    let mut mapper = mapper();
    let mut edges = Vec::new();
    for line in path.lines().filter(|l| !l.trim().is_empty()) {
        for obs in mapper.map_line(line).unwrap() {
            if let ObservationPayload::Model(payload) = &obs.body {
                edges.push((payload.effective.id.clone(), payload.effective.source));
            }
        }
    }
    edges
}

#[test]
fn real_walk_observes_the_resolved_gateway_ids() {
    // Baseline claude-opus-4-8, then sonnet resolves to the pinned o48 id, then
    // an explicit o50 gateway id. The sonnet switch resolves to the same o48 id
    // (pinned env), so it is NOT an edge; o50 is.
    let edges = replay(WALK);
    let ids: Vec<&str> = edges.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains(&"claude-opus-4-8"), "baseline: {ids:?}");
    assert!(
        ids.contains(&"model_hub/es1_orange_o50"),
        "gateway id edge: {ids:?}"
    );
}

#[test]
fn real_reject_records_emit_no_model_edges() {
    assert_eq!(replay(REJECT), Vec::<(String, EffortSource)>::new());
}

#[tokio::test]
async fn real_walk_switch_resolves_its_bridge_from_the_stdout_verdict() {
    use std::sync::Arc;
    use std::time::Duration;
    let bridge = Arc::new(remuda_driver::test_support::ModelBridgeHandle::new());
    let mut mapper = remuda_driver::test_support::mapper_with_model_bridge(
        bridge.clone(),
        "model-session",
        "2.1.272",
    );
    let generation = bridge.arm("model_hub/es1_orange_o50");
    let slash = WALK
        .lines()
        .find(|l| l.contains("<command-args>model_hub/es1_orange_o50</command-args>"))
        .expect("o50 slash");
    mapper.map_line(slash).unwrap();
    let stdout = WALK
        .lines()
        .find(|l| l.contains("Set model to `model_hub/es1_orange_o50`"))
        .expect("o50 stdout");
    mapper.map_line(stdout).unwrap();
    match bridge.wait(generation, Duration::from_secs(1)).await {
        Some(remuda_driver::model::ModelReadback::Applied(observed)) => {
            assert_eq!(observed.id, "model_hub/es1_orange_o50");
        }
        other => panic!("expected Applied o50, got {other:?}"),
    }
}

#[tokio::test]
async fn real_bogus_rejects_the_bridge_with_a_reason() {
    use std::sync::Arc;
    use std::time::Duration;
    let bridge = Arc::new(remuda_driver::test_support::ModelBridgeHandle::new());
    let mut mapper = remuda_driver::test_support::mapper_with_model_bridge(
        bridge.clone(),
        "model-session",
        "2.1.272",
    );
    let generation = bridge.arm("bogus-xyz-123");
    for line in REJECT.lines().take(2) {
        mapper.map_line(line).unwrap();
    }
    match bridge.wait(generation, Duration::from_secs(1)).await {
        Some(remuda_driver::model::ModelReadback::Rejected { reason }) => {
            assert_eq!(reason, "not-found");
        }
        other => panic!("expected not-found, got {other:?}"),
    }
}
