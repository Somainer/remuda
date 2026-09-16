//! Permission-mode transcript read-back on claude 2.1.273: `permission`
//! observations, `/plan` slash attribution, edge dedup, and correlation with a
//! pending Remuda switch — through the real [`TranscriptMapper`]. The
//! fixture records under `fixtures/permission-21273/` are verbatim lines
//! captured from a real claude 2.1.273 PTY (evidence permission-modes-1.md).

use remuda_driver::test_support::{
    PermissionBridgeHandle, mapper_with_permission_bridge,
};
use remuda_driver::TranscriptMapper;
use remuda_protocol::{
    ClaudePermissionMode, DriverKind, HostId, Id, InstanceId, ObservationPayload,
    PermissionSource, RunId,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const SESSION: &str = "perm-session";

fn mapper() -> TranscriptMapper {
    TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        SESSION.into(),
        "2.1.273".into(),
    )
}

fn mapper_with(
    handle: &Arc<PermissionBridgeHandle>,
    launch: Option<ClaudePermissionMode>,
) -> TranscriptMapper {
    mapper_with_permission_bridge(handle.clone(), SESSION, "2.1.273", launch)
}

fn permission_record(mode: &str) -> String {
    json!({
        "type": "permission-mode",
        "permissionMode": mode,
        "sessionId": SESSION,
    })
    .to_string()
}

fn mode_record(mode: &str) -> String {
    json!({
        "type": "mode",
        "mode": mode,
        "sessionId": SESSION,
    })
    .to_string()
}

fn plan_slash() -> String {
    json!({
        "type": "user",
        "uuid": "cmd-plan-1",
        "sessionId": SESSION,
        "message": {
            "role": "user",
            "content": "<local-command-caveat>Caveat: The messages below were generated \
                        by the user while running local commands.</local-command-caveat>\
                        <command-name>/plan</command-name>\
                        <command-message>plan</command-message>"
        }
    })
    .to_string()
}

#[derive(Debug, PartialEq)]
struct Edge {
    mode: String,
    source: PermissionSource,
    requested: Option<String>,
}

fn edges(out: &[remuda_protocol::Observation]) -> Vec<Edge> {
    out.iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::Permission(payload) => Some(Edge {
                mode: payload.effective.mode.clone(),
                source: payload.effective.source,
                requested: payload.requested.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn feed(mapper: &mut TranscriptMapper, line: &str) -> Vec<Edge> {
    edges(&mapper.map_line(line).expect("map"))
}

#[test]
fn mode_records_emit_edges_and_consecutive_duplicates_dedupe() {
    let mut mapper = mapper();
    assert_eq!(
        feed(&mut mapper, &permission_record("default")),
        vec![Edge {
            mode: "manual".into(),
            source: PermissionSource::Unknown,
            requested: None
        }]
    );
    // `mode: normal` is the TUI render mode, never a permission edge.
    assert!(feed(&mut mapper, &mode_record("normal")).is_empty());
    // Repeated mode emits nothing.
    assert!(feed(&mut mapper, &permission_record("default")).is_empty());
    assert_eq!(
        feed(&mut mapper, &permission_record("acceptEdits")),
        vec![Edge {
            mode: "acceptEdits".into(),
            source: PermissionSource::Unknown,
            requested: None
        }]
    );
    // TUI `default` spelling folds back to manual on return.
    assert_eq!(
        feed(&mut mapper, &permission_record("default"))
            .into_iter()
            .map(|edge| edge.mode)
            .collect::<Vec<_>>(),
        vec!["manual".to_string()]
    );
}

#[test]
fn plan_slash_attributes_the_next_edge_to_the_terminal() {
    let mut mapper = mapper();
    feed(&mut mapper, &permission_record("default"));
    assert!(feed(&mut mapper, &plan_slash()).is_empty());
    assert_eq!(
        feed(&mut mapper, &permission_record("plan")),
        vec![Edge {
            mode: "plan".into(),
            source: PermissionSource::Slash,
            requested: None
        }]
    );
}

#[test]
fn launch_mode_marks_the_first_matching_edge_as_launch() {
    let handle = Arc::new(PermissionBridgeHandle::new(false));
    let mut mapper = mapper_with(&handle, Some(ClaudePermissionMode::Plan));
    assert_eq!(
        feed(&mut mapper, &permission_record("plan")),
        vec![Edge {
            mode: "plan".into(),
            source: PermissionSource::Launch,
            requested: None
        }]
    );
}

#[tokio::test]
async fn a_remuda_cycle_edge_is_attributed_remuda_and_settles_the_switch() {
    let handle = Arc::new(PermissionBridgeHandle::new(false));
    let mut mapper = mapper_with(&handle, None);
    feed(&mut mapper, &permission_record("default"));
    // The driver armed a walk to auto (three shift+tabs).
    let generation = handle.arm(ClaudePermissionMode::Auto);
    assert_eq!(
        feed(&mut mapper, &permission_record("auto")),
        vec![Edge {
            mode: "auto".into(),
            source: PermissionSource::Remuda,
            requested: Some("auto".into())
        }]
    );
    // The bridge rendezvous settles on the transcript edge.
    assert_eq!(
        handle.wait(generation, Duration::from_secs(1)).await,
        Some(ClaudePermissionMode::Auto)
    );
}

#[test]
fn verbatim_21273_walk_fixture_replays_every_mode() {
    let raw = include_str!("fixtures/permission-21273/permission-walk-21273.jsonl");
    let mut mapper = mapper();
    let mut seen = Vec::new();
    for line in raw.lines() {
        seen.extend(feed(&mut mapper, line));
    }
    let modes: Vec<_> = seen.iter().map(|edge| edge.mode.clone()).collect();
    // default(manual) -> /plan -> plan -> acceptEdits -> auto -> default
    assert_eq!(
        modes,
        vec!["manual", "plan", "acceptEdits", "auto", "manual"]
    );
    // The plan edge after /plan is slash-attributed.
    assert_eq!(seen[1].source, PermissionSource::Slash);
    // The bare shift+tab edges after it are not.
    assert_eq!(seen[2].source, PermissionSource::Unknown);
    assert_eq!(seen[3].source, PermissionSource::Unknown);
}

#[test]
fn verbatim_21273_records_cover_all_six_modes() {
    // Every mode word the binary actually writes, captured verbatim across
    // boot (`--permission-mode dontAsk`) and wheel sessions.
    let raw = include_str!(
        "fixtures/permission-21273/permission-records-verbatim-21273.jsonl"
    );
    let mut modes = std::collections::BTreeSet::new();
    for line in raw.lines() {
        let value: Value = serde_json::from_str(line).expect("json");
        modes.insert(value["permissionMode"].as_str().expect("word").to_string());
    }
    assert!(modes.contains("default"));
    assert!(modes.contains("acceptEdits"));
    assert!(modes.contains("plan"));
    assert!(modes.contains("auto"));
    assert!(modes.contains("bypassPermissions"));
    assert!(modes.contains("dontAsk"));
}

#[test]
fn dontask_launch_fixture_records_the_launch_mode() {
    let raw = include_str!(
        "fixtures/permission-21273/permission-launch-dontask-21273.jsonl"
    );
    let handle = Arc::new(PermissionBridgeHandle::new(false));
    let mut mapper = mapper_with(&handle, Some(ClaudePermissionMode::DontAsk));
    let mut seen = Vec::new();
    for line in raw.lines() {
        seen.extend(feed(&mut mapper, line));
    }
    assert_eq!(
        seen.into_iter()
            .map(|edge| (edge.mode, edge.source))
            .collect::<Vec<_>>(),
        vec![("dontAsk".to_string(), PermissionSource::Launch)]
    );
}
