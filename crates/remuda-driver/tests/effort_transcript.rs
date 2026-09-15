//! §9.1 transcript effort read-back: `effort` observations, slash detection,
//! and dedupe — driven through the real [`TranscriptMapper`] with synthetic
//! records shaped exactly like claude 2.1.221 writes them.

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
