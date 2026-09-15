//! §9.1 transcript effort read-back: `effort` observations, slash detection,
//! and dedupe — driven through the real [`TranscriptMapper`] with synthetic
//! records shaped exactly like claude 2.1.221 writes them, plus a verbatim
//! replay of a real claude 2.1.272 PTY session (`fixtures/effort-21272/`,
//! measured by `examples/effort_probe.rs`, evidence effort-sync-2.md).

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
        "effort-session".into(),
        "2.1.221".into(),
    )
}

fn assistant(effort: Option<&str>, n: u64) -> String {
    let mut record = json!({
        "type": "assistant",
        "uuid": format!("msg-{n}"),
        "sessionId": "effort-session",
        "message": {
            "id": format!("msg-{n}"),
            "role": "assistant",
            "type": "message",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
        },
        "effort": effort,
        "perTurnEffort": null,
    });
    if effort.is_none() {
        record.as_object_mut().unwrap().remove("effort");
        record.as_object_mut().unwrap().remove("perTurnEffort");
    }
    record.to_string()
}

fn slash(word: &str, n: u64) -> String {
    json!({
        "type": "user",
        "uuid": format!("cmd-{n}"),
        "sessionId": "effort-session",
        "message": {
            "role": "user",
            "content": format!(
                "<command-name>/effort</command-name>\n<command-message>effort</command-message>\n\
                 <command-args>{word}</command-args>"
            )
        }
    })
    .to_string()
}

fn effort_edges(out: &[remuda_protocol::Observation]) -> Vec<(String, EffortSource, Option<bool>)> {
    out.iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::Effort(payload) => Some((
                payload.effective.name.wire().to_string(),
                payload.effective.source,
                payload.effective.ultracode,
            )),
            _ => None,
        })
        .collect()
}

trait WireName {
    fn wire(&self) -> &'static str;
}
impl WireName for remuda_protocol::EffortName {
    fn wire(&self) -> &'static str {
        match self {
            remuda_protocol::EffortName::Low => "low",
            remuda_protocol::EffortName::Medium => "medium",
            remuda_protocol::EffortName::High => "high",
            remuda_protocol::EffortName::Xhigh => "xhigh",
            remuda_protocol::EffortName::Max => "max",
            remuda_protocol::EffortName::Ultra => "ultra",
            // Legacy input word; never observed on a Claude record.
            remuda_protocol::EffortName::Minimal => "minimal",
        }
    }
}

#[test]
fn the_first_assistant_record_emits_once_then_identical_records_are_deduped() {
    let mut mapper = mapper();
    let out = mapper.map_line(&assistant(Some("high"), 1)).expect("map");
    assert_eq!(
        effort_edges(&out),
        vec![("high".to_string(), EffortSource::Unknown, None)]
    );
    // Two more identical high records emit nothing.
    assert!(
        mapper
            .map_line(&assistant(Some("high"), 2))
            .expect("map")
            .iter()
            .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_)))
    );
    assert!(
        mapper
            .map_line(&assistant(Some("high"), 3))
            .expect("map")
            .iter()
            .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_)))
    );
}

#[test]
fn a_typed_effort_slash_marks_the_next_new_level_as_source_slash() {
    let mut mapper = mapper();
    mapper.map_line(&assistant(Some("high"), 1)).expect("map");
    mapper.map_line(&slash("xhigh", 1)).expect("map");
    let out = mapper.map_line(&assistant(Some("xhigh"), 2)).expect("map");
    assert_eq!(
        effort_edges(&out),
        vec![("xhigh".to_string(), EffortSource::Slash, None)]
    );
}

#[test]
fn ultracode_slash_reads_back_as_xhigh_with_the_flag() {
    let mut mapper = mapper();
    mapper.map_line(&assistant(Some("high"), 1)).expect("map");
    mapper.map_line(&slash("ultracode", 1)).expect("map");
    let out = mapper.map_line(&assistant(Some("xhigh"), 2)).expect("map");
    assert_eq!(
        effort_edges(&out),
        vec![("xhigh".to_string(), EffortSource::Slash, Some(true))]
    );
}

#[test]
fn per_turn_effort_is_the_fallback_field() {
    let mut record = serde_json::from_str::<serde_json::Value>(&assistant(None, 1)).unwrap();
    record["perTurnEffort"] = json!("medium");
    let out = mapper().map_line(&record.to_string()).expect("map");
    assert_eq!(
        effort_edges(&out),
        vec![("medium".to_string(), EffortSource::Unknown, None)]
    );
}

#[test]
fn an_unrelated_slash_command_does_not_attribute_effort() {
    let mut mapper = mapper();
    mapper.map_line(&assistant(Some("high"), 1)).expect("map");
    mapper
        .map_line(
            &json!({
                "type": "user",
                "uuid": "cmd-x",
                "sessionId": "effort-session",
                "message": {"role": "user", "content": "<command-name>/clear</command-name>"}
            })
            .to_string(),
        )
        .expect("map");
    // A later high record is unchanged and stays silent.
    let out = mapper.map_line(&assistant(Some("high"), 2)).expect("map");
    assert!(effort_edges(&out).is_empty());
}

// ───────── Real claude 2.1.272 PTY session, replayed verbatim ───────────────

const WALK_21272: &str = include_str!("fixtures/effort-21272/effort-walk-21272.jsonl");
const REJECT_21272: &str = include_str!("fixtures/effort-21272/effort-reject-21272.jsonl");

#[derive(Debug, PartialEq)]
struct EffortEdge {
    name: String,
    source: EffortSource,
    ultracode: Option<bool>,
}

fn replay(path: &str) -> Vec<EffortEdge> {
    let mut mapper = mapper();
    let mut edges = Vec::new();
    for line in path.lines().filter(|l| !l.trim().is_empty()) {
        for obs in mapper.map_line(line).expect("map") {
            if let ObservationPayload::Effort(payload) = &obs.body {
                edges.push(EffortEdge {
                    name: payload.effective.name.wire().to_string(),
                    source: payload.effective.source,
                    ultracode: payload.effective.ultracode,
                });
            }
        }
    }
    edges
}

#[test]
fn real_21272_walk_settles_every_level_from_the_stdout_verdict() {
    // The real walk: low baseline assistant; xhigh accepted; max dismissed
    // (Kept); ultracode accepted; high accepted (ultra exits).
    let edges = replay(WALK_21272);
    let words: Vec<_> = edges
        .iter()
        .map(|e| (e.name.as_str(), e.ultracode))
        .collect();
    // The xhigh edge arrives from the command verdict — before the next
    // assistant record exists — and the ultracode edge carries the flag.
    assert!(
        words.contains(&("xhigh", Some(false))),
        "xhigh stdout accept: {words:?}"
    );
    assert!(
        words.contains(&("xhigh", Some(true))),
        "ultracode stdout accept: {words:?}"
    );
    assert!(
        words.contains(&("high", Some(false))),
        "high stdout accept clears the flag: {words:?}"
    );
    // A dismissed dialog must never emit max.
    assert!(
        !words.iter().any(|(name, _)| name == &"max"),
        "the Esc-on-dialog max never takes effect: {words:?}"
    );
    // The post-ultracode assistant record carries xhigh and the flag stays
    // latched (it was already emitted at the verdict, so this is deduped — the
    // latched flag is what keeps a later xhigh record honest).
}

#[test]
fn real_21272_reject_records_emit_no_effort_edges() {
    // Esc on the "Change effort to max" dialog → Kept; /effort bogus → Invalid.
    // Neither changes the effective level, so no effort observation is emitted.
    assert_eq!(replay(REJECT_21272), Vec::<EffortEdge>::new());
}

#[tokio::test]
async fn real_21272_ultracode_switch_resolves_its_bridge_from_stdout_not_the_turn() {
    // Wire the mapper to a bridge the way the driver does, arm an ultracode
    // switch, and replay ONLY the slash + stdout pair: the generation resolves
    // Applied(xhigh, ultracode:true) without any assistant record.
    use std::sync::Arc;
    use std::time::Duration;
    let bridge = Arc::new(remuda_driver::test_support::Bridge::new());
    let mut mapper = mapper_with_bridge(&bridge);
    let generation = bridge.arm_ultracode();
    let slash = WALK_21272
        .lines()
        .find(|l| l.contains("<command-args>ultracode</command-args>"))
        .expect("ultracode slash record");
    mapper.map_line(slash).expect("map");
    let stdout = WALK_21272
        .lines()
        .find(|l| l.contains("Set effort level to ultracode"))
        .expect("ultracode stdout record");
    mapper.map_line(stdout).expect("map");
    let verdict = bridge.wait(generation, Duration::from_secs(1)).await;
    match verdict {
        Some(remuda_driver::effort::Readback::Applied(observed)) => {
            assert_eq!(observed.name, remuda_protocol::EffortName::Xhigh);
            assert_eq!(observed.ultracode, Some(true));
        }
        other => panic!("expected Applied ultracode, got {other:?}"),
    }
}

#[tokio::test]
async fn real_21272_dismissed_dialog_rejects_the_bridge_with_a_reason() {
    use std::sync::Arc;
    use std::time::Duration;
    let bridge = Arc::new(remuda_driver::test_support::Bridge::new());
    let mut mapper = mapper_with_bridge(&bridge);
    let generation = bridge.arm_max();
    for line in REJECT_21272.lines().take(2) {
        mapper.map_line(line).expect("map");
    }
    match bridge.wait(generation, Duration::from_secs(1)).await {
        Some(remuda_driver::effort::Readback::Rejected { reason }) => {
            assert_eq!(reason, "dialog-kept");
        }
        other => panic!("expected Kept rejection, got {other:?}"),
    }
}

fn mapper_with_bridge(
    bridge: &std::sync::Arc<remuda_driver::test_support::Bridge>,
) -> TranscriptMapper {
    remuda_driver::test_support::mapper_with_bridge(bridge.clone(), "effort-session", "2.1.272")
}
