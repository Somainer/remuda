//! One fixture replay for grok ACP stdio (`D-013` freeze).

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

#[test]
fn grok_stdio_session_replay() {
    let body =
        std::fs::read_to_string(fixture("grok-acp-session.jsonl")).expect("grok-acp-session.jsonl");
    let mut saw_initialize = false;
    let mut saw_session_new = false;
    let mut yolo = false;
    let mut saw_prompt_ok = false;
    let mut stop_end_turn = false;
    let mut saw_ok_chunk = false;
    let mut saw_write_tool = false;
    let mut request_permission = 0usize;
    let mut kinds = Vec::new();

    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        let parsed = parse_line(line).unwrap_or_else(|err| panic!("{err} in {line}"));
        let ParsedLine::Capture { rpc, .. } = parsed else {
            panic!("expected capture envelope");
        };
        if rpc.get("method").and_then(|m| m.as_str()) == Some("session/request_permission") {
            request_permission += 1;
        }
        match parse_line(line).unwrap().classify().unwrap() {
            WireEvent::Request { method, .. } if method == "initialize" => {
                saw_initialize = true;
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
            _ => {}
        }
    }

    assert!(saw_initialize);
    assert!(!client_capabilities_declare_fs_or_terminal(
        &remuda_acp_wire::initialize_params("runtime", "0.1.0")
    ));
    assert!(saw_session_new);
    assert!(yolo);
    assert!(saw_prompt_ok);
    assert!(stop_end_turn);
    assert!(saw_ok_chunk);
    assert!(saw_write_tool);
    assert_eq!(request_permission, 0);
    assert!(kinds.contains(&SessionUpdateKind::AgentThoughtChunk));
    assert!(kinds.contains(&SessionUpdateKind::ToolCallUpdate));
}
