//! Fixture replay for grok ACP captures and the synthetic unknown-tag file.

use std::path::PathBuf;

use remuda_acp_wire::{
    ParsedLine, SessionUpdateKind, WireEvent, client_capabilities_declare_fs_or_terminal,
    parse_line,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn load_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(fixture(name))
        .unwrap_or_else(|err| panic!("read {name}: {err}"))
        .lines()
        .map(ToOwned::to_owned)
        .filter(|line| !line.trim().is_empty())
        .collect()
}

#[test]
fn grok_stdio_session_replay() {
    let mut saw_initialize = false;
    let mut saw_empty_caps = false;
    let mut saw_session_new = false;
    let mut yolo = false;
    let mut saw_prompt_ok = false;
    let mut stop_end_turn = false;
    let mut saw_ok_chunk = false;
    let mut saw_write_tool = false;
    let mut saw_xai = false;
    let mut saw_load = false;
    let mut request_permission = 0usize;
    let mut kinds = Vec::new();

    for line in load_lines("grok-acp-session.jsonl") {
        let parsed = parse_line(&line).unwrap_or_else(|err| panic!("{err} in {line}"));
        let ParsedLine::Capture { rpc, .. } = parsed else {
            panic!("expected capture envelope");
        };
        if rpc.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
            request_permission += 1;
        }
        match parse_line(&line).unwrap().classify().unwrap() {
            WireEvent::Request { method, params, .. } if method == "initialize" => {
                saw_initialize = true;
                // Probe declared fs/terminal; this crate must not copy that.
                if !client_capabilities_declare_fs_or_terminal(&params) {
                    saw_empty_caps = true;
                }
            }
            WireEvent::Request { method, params, .. } if method == "session/new" => {
                saw_session_new = true;
                yolo = params
                    .pointer("/_meta/yoloMode")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true);
                assert_eq!(params["cwd"], "/tmp/hh-probe-grok");
                assert_eq!(params["mcpServers"], serde_json::json!([]));
            }
            WireEvent::Request { method, params, .. } if method == "session/prompt" => {
                if params
                    .pointer("/prompt/0/text")
                    .and_then(serde_json::Value::as_str)
                    == Some("Reply with exactly OK")
                {
                    saw_prompt_ok = true;
                }
            }
            WireEvent::Request { method, .. } if method == "session/load" => {
                saw_load = true;
            }
            WireEvent::Response { result, .. } => {
                if result.get("stopReason").and_then(|s| s.as_str()) == Some("end_turn") {
                    stop_end_turn = true;
                }
            }
            WireEvent::SessionUpdate {
                update_kind,
                update,
                ..
            } => {
                kinds.push(update_kind.clone());
                if update_kind == SessionUpdateKind::AgentMessageChunk
                    && remuda_acp_wire::chunk_text(&update) == Some("OK")
                {
                    saw_ok_chunk = true;
                }
                if update_kind == SessionUpdateKind::ToolCall
                    && remuda_acp_wire::tool_call_title(&update) == Some("write")
                {
                    saw_write_tool = true;
                }
            }
            WireEvent::Ext { method, .. } if method.starts_with("_x.ai/") => {
                saw_xai = true;
            }
            _ => {}
        }
    }

    assert!(saw_initialize);
    assert!(
        !saw_empty_caps,
        "probe capture advertised fs/terminal; crate initialize_params must not"
    );
    assert!(!client_capabilities_declare_fs_or_terminal(
        &remuda_acp_wire::initialize_params("runtime", "0.1.0")
    ));
    assert!(saw_session_new);
    assert!(yolo);
    assert!(saw_prompt_ok);
    assert!(stop_end_turn);
    assert!(saw_ok_chunk);
    assert!(saw_write_tool);
    assert!(saw_xai);
    assert!(saw_load);
    assert_eq!(request_permission, 0);
    assert!(kinds.contains(&SessionUpdateKind::AgentThoughtChunk));
    assert!(kinds.contains(&SessionUpdateKind::ToolCallUpdate));
}

#[test]
fn grok_serve_ws_replay() {
    let mut saw_ws = false;
    let mut saw_init = false;
    for line in load_lines("grok-acp-serve.jsonl") {
        match parse_line(&line).unwrap() {
            ParsedLine::Capture { meta, rpc } => {
                if meta.transport == remuda_acp_wire::TransportKind::Ws {
                    saw_ws = true;
                }
                if rpc.get("method").and_then(|m| m.as_str()) == Some("initialize") {
                    saw_init = true;
                }
            }
            other => panic!("expected capture: {other:?}"),
        }
    }
    assert!(saw_ws);
    assert!(saw_init);
}

#[test]
fn headless_streaming_json_is_not_acp() {
    let mut other = 0usize;
    for line in load_lines("grok-headless-streaming-json.jsonl") {
        match parse_line(&line).unwrap() {
            ParsedLine::Other(value) => {
                other += 1;
                assert!(value.get("type").is_some());
            }
            ParsedLine::Rpc(_) | ParsedLine::Capture { .. } => {
                panic!("headless line parsed as ACP")
            }
        }
    }
    assert!(other > 0);
}

#[test]
fn synthetic_unknown_and_plan() {
    let mut saw_plan = false;
    let mut saw_unknown_update = false;
    let mut saw_unknown_method = false;
    let mut saw_ext = false;
    let mut saw_rewritten = false;
    for line in load_lines("synthetic-unknown.jsonl") {
        match parse_line(&line).unwrap().classify().unwrap() {
            WireEvent::SessionUpdate {
                update_kind: SessionUpdateKind::Plan,
                ..
            } => {
                saw_plan = true;
            }
            WireEvent::SessionUpdate {
                update_kind: SessionUpdateKind::Unknown(tag),
                ..
            } => {
                assert_eq!(tag, "hook_execution");
                saw_unknown_update = true;
            }
            WireEvent::Unknown { method, .. } if method == "totally/unknown" => {
                saw_unknown_method = true;
            }
            WireEvent::Ext { method, .. } if method == "_x.ai/session_notification" => {
                saw_ext = true;
            }
            WireEvent::Ext { method, .. } if method == "_x.ai/fs/list" => {
                saw_rewritten = true;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(saw_plan);
    assert!(saw_unknown_update);
    assert!(saw_unknown_method);
    assert!(saw_ext);
    assert!(saw_rewritten);
}

#[test]
fn crate_initialize_json_omits_fs_terminal_true() {
    let line = remuda_acp_wire::encode_request(
        1,
        "initialize",
        remuda_acp_wire::initialize_params("runtime", remuda_acp_wire::adapter_version()),
    )
    .unwrap();
    assert!(line.contains("\"clientCapabilities\":{}"));
    assert!(!line.contains("\"readTextFile\":true"));
    assert!(!line.contains("\"writeTextFile\":true"));
    assert!(!line.contains("\"terminal\":true"));
}
