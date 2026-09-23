//! Fixture decode tests. Probe jsonl must not land in `Unknown`.

use remuda_claude_wire::{ControlRequest, Inbound, Outbound, SystemMessage, UserContent};
use serde_json::Value;
use std::fs;
use std::path::Path;

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_objects(path: &Path) -> Vec<(usize, Value)> {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        return vec![(1, serde_json::from_str(&raw).expect("json"))];
    }
    raw.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                None
            } else {
                Some((
                    i + 1,
                    serde_json::from_str(line)
                        .unwrap_or_else(|e| panic!("{}:{}: {e}", path.display(), i + 1)),
                ))
            }
        })
        .collect()
}

#[derive(Debug)]
enum Decoded {
    In(Inbound),
    Out(Outbound),
}

fn decode(value: Value) -> Decoded {
    if value.get("_dir").and_then(Value::as_str) == Some("in") {
        Decoded::In(Inbound::from_value(value))
    } else {
        Decoded::Out(Outbound::from_value(value))
    }
}

fn unknown_note(decoded: &Decoded) -> Option<String> {
    match decoded {
        Decoded::Out(Outbound::Unknown(v)) => Some(format!(
            "unknown type {}",
            v.get("type").and_then(Value::as_str).unwrap_or("?")
        )),
        Decoded::Out(Outbound::System(SystemMessage::Unknown(v))) => Some(format!(
            "unknown system subtype {}",
            v.get("subtype").and_then(Value::as_str).unwrap_or("?")
        )),
        Decoded::Out(Outbound::ControlRequest(env)) => match &env.request {
            ControlRequest::Unknown(v) => Some(format!(
                "unknown control subtype {}",
                v.get("subtype").and_then(Value::as_str).unwrap_or("?")
            )),
            _ => None,
        },
        Decoded::In(Inbound::Unknown(v)) => Some(format!(
            "unknown inbound type {}",
            v.get("type").and_then(Value::as_str).unwrap_or("?")
        )),
        Decoded::In(Inbound::ControlRequest(env)) => match &env.request {
            ControlRequest::Unknown(v) => Some(format!(
                "unknown inbound control subtype {}",
                v.get("subtype").and_then(Value::as_str).unwrap_or("?")
            )),
            _ => None,
        },
        _ => None,
    }
}

fn decode_named(name: &str) -> Vec<(usize, Decoded, Option<String>)> {
    let path = fixtures_dir().join(name);
    load_objects(&path)
        .into_iter()
        .map(|(line, value)| {
            let decoded = decode(value);
            let note = unknown_note(&decoded);
            (line, decoded, note)
        })
        .collect()
}

fn assert_no_unknown(name: &str) {
    for (line, decoded, note) in decode_named(name) {
        assert!(
            note.is_none(),
            "{name}:{line} fell through to Unknown ({note:?}) as {decoded:?}"
        );
    }
}

#[test]
fn probe_fixtures_decode_without_unknown() {
    for name in [
        "claude-p-init.json",
        "claude-permission-host-allow.jsonl",
        "claude-permission-host-deny.jsonl",
        "claude-askuser.jsonl",
        "claude-exit-plan-mode-allow.jsonl",
        "claude-exit-plan-mode-deny.jsonl",
        "claude-hook-permission.jsonl",
        "claude-hook-permission-allow.jsonl",
        "claude-hook-permission-deny.jsonl",
        "workflow-control-plane-s4.jsonl",
    ] {
        assert_no_unknown(name);
    }
}

#[test]
fn host_allow_fixture_has_can_use_tool_and_camel_case_allow() {
    let mut saw_tool = false;
    let mut saw_allow = false;
    for (_line, decoded, _) in decode_named("claude-permission-host-allow.jsonl") {
        match decoded {
            Decoded::Out(Outbound::ControlRequest(env)) => {
                if let ControlRequest::CanUseTool(req) = env.request {
                    assert_eq!(req.tool_name, "Bash");
                    assert_eq!(env.request_id, "3933a12a-db1f-4d3b-b345-4602adab8195");
                    assert!(req.blocked_path.is_some() || req.blocked_paths.is_some());
                    saw_tool = true;
                }
            }
            Decoded::In(Inbound::ControlResponse(env)) => {
                let perm = env.response.permission_result().expect("permission");
                match perm {
                    remuda_claude_wire::PermissionResult::Allow { updated_input, .. } => {
                        assert_eq!(
                            env.response.request_id(),
                            "3933a12a-db1f-4d3b-b345-4602adab8195"
                        );
                        assert!(updated_input.get("command").is_some());
                        saw_allow = true;
                    }
                    other => panic!("expected allow, got {other:?}"),
                }
            }
            _ => {}
        }
    }
    assert!(saw_tool && saw_allow);
}

#[test]
fn askuser_fixture_is_can_use_tool() {
    let mut found = false;
    for (_line, decoded, _) in decode_named("claude-askuser.jsonl") {
        if let Decoded::Out(Outbound::ControlRequest(env)) = decoded
            && let ControlRequest::CanUseTool(req) = env.request
            && req.tool_name == "AskUserQuestion"
        {
            found = true;
            assert_eq!(req.requires_user_interaction, Some(true));
        }
    }
    assert!(found);
}

#[test]
fn workflow_fixture_keeps_both_results_and_task_events() {
    let mut results = 0u32;
    let mut started = false;
    let mut progress = false;
    let mut updated = false;
    let mut notified = false;
    let mut snapshot = false;
    for (_line, decoded, _) in decode_named("workflow-control-plane-s4.jsonl") {
        match decoded {
            Decoded::Out(Outbound::Result(_)) => results += 1,
            Decoded::Out(Outbound::System(SystemMessage::TaskStarted(_))) => started = true,
            Decoded::Out(Outbound::System(SystemMessage::TaskProgress(_))) => progress = true,
            Decoded::Out(Outbound::System(SystemMessage::TaskUpdated(_))) => updated = true,
            Decoded::Out(Outbound::System(SystemMessage::TaskNotification(_))) => notified = true,
            Decoded::Out(Outbound::System(SystemMessage::BackgroundTasksChanged(_))) => {
                snapshot = true
            }
            _ => {}
        }
    }
    assert!(
        results >= 2,
        "Workflow emits more than one result, got {results}"
    );
    assert!(started && progress && updated && notified && snapshot);
}

#[test]
fn synthetic_known_frames_and_documented_unknowns() {
    let mut unknown_type = false;
    let mut unknown_system = false;
    let mut unknown_control = false;
    let mut keep_alive = false;
    let mut stream = false;
    let mut init_in = false;
    let mut user_blocks = false;
    for (line, decoded, note) in decode_named("synthetic-coverage.jsonl") {
        match decoded {
            Decoded::Out(Outbound::KeepAlive) => keep_alive = true,
            Decoded::Out(Outbound::StreamEvent(_)) => stream = true,
            Decoded::Out(Outbound::Unknown(v)) => {
                assert_eq!(v["type"], "future_frame");
                unknown_type = true;
            }
            Decoded::Out(Outbound::System(SystemMessage::Unknown(v))) => {
                assert_eq!(v["subtype"], "brand_new_subtype");
                unknown_system = true;
            }
            Decoded::Out(Outbound::ControlRequest(env)) => match env.request {
                ControlRequest::Unknown(v) => {
                    assert_eq!(v["subtype"], "not_a_real_subtype");
                    unknown_control = true;
                }
                ControlRequest::CanUseTool(req) => {
                    assert!(req.blocked_paths.is_some());
                }
                ControlRequest::HookCallback(_) | ControlRequest::McpMessage(_) => {}
                other => panic!("line {line}: unexpected control {other:?}"),
            },
            Decoded::In(Inbound::ControlRequest(env)) => match env.request {
                ControlRequest::Initialize(body) => {
                    assert!(body.hooks.is_some());
                    assert_eq!(body.per_task_stop_affordance, Some(true));
                    init_in = true;
                }
                ControlRequest::Interrupt { .. }
                | ControlRequest::SetPermissionMode { .. }
                | ControlRequest::SetModel { .. } => {}
                other => panic!("line {line}: unexpected inbound control {other:?}"),
            },
            Decoded::In(Inbound::User(user)) => {
                assert!(matches!(user.message.content, UserContent::Blocks(_)));
                user_blocks = true;
            }
            Decoded::Out(_) | Decoded::In(_) => {
                assert!(note.is_none(), "line {line}: unexpected unknown {note:?}");
            }
        }
    }
    assert!(
        keep_alive
            && stream
            && init_in
            && user_blocks
            && unknown_type
            && unknown_system
            && unknown_control
    );
}

#[test]
fn init_fixture_is_system_init_with_workflow_tool() {
    let decoded = decode_named("claude-p-init.json");
    assert_eq!(decoded.len(), 1);
    match &decoded[0].1 {
        Decoded::Out(Outbound::System(SystemMessage::Init(init))) => {
            let tools = init.tools.as_ref().expect("tools");
            assert!(tools.iter().any(|t| t == "Workflow"));
        }
        other => panic!("expected system/init, got {other:?}"),
    }
}
