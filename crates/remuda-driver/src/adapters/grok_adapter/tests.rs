//! Tests against the captured real grok 1.0.30 session and synthetic frames.

use super::*;
use remuda_protocol::ObservationPayload;
use serde_json::json;
use std::fs;
use std::path::Path;

const REAL_UPDATES: &str = include_str!("../../../tests/fixtures/grok/tui-updates.jsonl");
const REAL_EVENTS: &str = include_str!("../../../tests/fixtures/grok/tui-events.jsonl");
const REAL_REGISTRY: &str = include_str!("../../../tests/fixtures/grok/active-sessions.json");

fn write_real_session(dir: &Path, cwd: &Path, pid: u32, session: &str) -> GrokAdapter {
    let _ = cwd;
    let encoded = crate::grok_session::encode_session_cwd(cwd);
    let session_dir = dir.join("sessions").join(&encoded).join(session);
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("updates.jsonl"), REAL_UPDATES).unwrap();
    fs::write(session_dir.join("events.jsonl"), REAL_EVENTS).unwrap();
    fs::write(session_dir.join("usage.json"), "{}\n").unwrap();
    // Registry carries the fixture cwd, which we repoint at our temp cwd by
    // rewriting only that field.
    let registry: Value = serde_json::from_str(REAL_REGISTRY.trim()).unwrap();
    let mut registry = registry;
    registry[0]["cwd"] = Value::String(cwd.to_string_lossy().into_owned());
    registry[0]["pid"] = Value::Number(pid.into());
    fs::write(
        dir.join("active_sessions.json"),
        serde_json::to_vec(&registry).unwrap(),
    )
    .unwrap();
    GrokAdapter::new(AdapterHome {
        home: dir.to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: Some(pid),
    })
}

fn turn_names(observations: &[AdapterObservation]) -> Vec<String> {
    observations
        .iter()
        .filter_map(|observed| match &observed.payload {
            ObservationPayload::Lifecycle(box_payload) => match box_payload.as_ref() {
                remuda_protocol::LifecyclePayload::Native(native)
                    if native.topic == remuda_protocol::LifecycleTopic::Turn =>
                {
                    Some(native.native_name.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn the_real_session_has_seven_turn_boundaries_with_cancel_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let mut adapter = write_real_session(
        dir.path(),
        &cwd,
        24069,
        "01a09c24-46ef-7a03-9c89-88f1bc00bd0c",
    );
    let observations = adapter.poll().unwrap();
    let names = turn_names(&observations);
    assert_eq!(
        names
            .iter()
            .filter(|name| name.as_str() == "turn_started")
            .count(),
        7
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| name.as_str() == "turn_ended")
            .count(),
        7
    );
    // 5 completed, 2 cancelled (ctrl_c + send_now).
    let cancelled = observations
        .iter()
        .filter(|observed| {
            let ObservationPayload::Lifecycle(box_payload) = &observed.payload else {
                return false;
            };
            matches!(box_payload.as_ref(),
            remuda_protocol::LifecyclePayload::Native(n)
                if n.native_name == "turn_ended"
                    && n.related_ids.get("outcome").map(String::as_str) == Some("cancelled"))
        })
        .count();
    assert_eq!(cancelled, 2);
}

#[test]
fn chunks_stream_append_then_close_with_the_full_text() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let session = "sess-1";
    let encoded = crate::grok_session::encode_session_cwd(&cwd);
    let session_dir = dir.path().join("sessions").join(&encoded).join(session);
    fs::create_dir_all(&session_dir).unwrap();
    let updates = r#"
{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"SPIKE_"},"_meta":{"promptId":"p1","chunkId":1}}}}
{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"COMPLETE"},"_meta":{"promptId":"p1","chunkId":2}}}}
{"method":"_x.ai/session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","stop_reason":"end_turn","elapsed_ms":12}}}
"#;
    let events = r#"
{"ts":"t","type":"turn_started","turn_number":0}
{"ts":"t","type":"turn_ended","outcome":"completed"}
"#;
    fs::write(session_dir.join("updates.jsonl"), updates).unwrap();
    fs::write(session_dir.join("events.jsonl"), events).unwrap();
    fs::write(session_dir.join("usage.json"), "{}\n").unwrap();
    fs::write(
        dir.path().join("active_sessions.json"),
        serde_json::to_string(&json!([
            {"session_id": session, "pid": 11, "cwd": cwd.to_string_lossy(), "opened_at": "t"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let mut adapter = GrokAdapter::new(AdapterHome {
        home: dir.path().to_path_buf(),
        cwd,
        pid: Some(11),
    });
    let observed = adapter.poll().unwrap();
    let messages: Vec<_> = observed
        .iter()
        .filter_map(|o| match &o.payload {
            ObservationPayload::Message(message) => Some(message),
            _ => None,
        })
        .collect();
    // open chunk, append chunk, close snapshot.
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0].mutation.operation,
        remuda_protocol::MutationOperation::Open
    );
    assert_eq!(
        messages[1].mutation.operation,
        remuda_protocol::MutationOperation::Append
    );
    assert_eq!(
        messages[2].mutation.operation,
        remuda_protocol::MutationOperation::Close
    );
    assert_eq!(
        messages[2].blocks[0],
        remuda_protocol::ContentBlock::Text(Box::new(remuda_protocol::TextBlock {
            text: "SPIKE_COMPLETE".into()
        }))
    );
    assert_eq!(messages[2].status, ContentStatus::Complete);
}

#[test]
fn a_cancelled_turn_closes_its_message_as_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let session = "sess-2";
    let encoded = crate::grok_session::encode_session_cwd(&cwd);
    let session_dir = dir.path().join("sessions").join(&encoded).join(session);
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("updates.jsonl"),
        r#"
{"method":"session/update","params":{"sessionId":"sess-2","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"partial"},"_meta":{"promptId":"p2","chunkId":1}}}}
{"method":"_x.ai/session/update","params":{"sessionId":"sess-2","update":{"sessionUpdate":"turn_completed","prompt_id":"p2","stop_reason":"cancelled","elapsed_ms":3}}}
"#,
    )
    .unwrap();
    fs::write(
        session_dir.join("events.jsonl"),
        r#"
{"ts":"t","type":"turn_started","turn_number":0}
{"ts":"t","type":"turn_ended","outcome":"cancelled","cancellation_category":"mid_turn_abort","cancellation_context":{"trigger":"ctrl_c"}}
"#,
    )
    .unwrap();
    fs::write(session_dir.join("usage.json"), "{}\n").unwrap();
    fs::write(
        dir.path().join("active_sessions.json"),
        serde_json::to_string(&json!([
            {"session_id": session, "pid": 12, "cwd": cwd.to_string_lossy(), "opened_at": "t"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let mut adapter = GrokAdapter::new(AdapterHome {
        home: dir.path().to_path_buf(),
        cwd,
        pid: Some(12),
    });
    let observed = adapter.poll().unwrap();
    let close = observed
        .iter()
        .find_map(|o| match &o.payload {
            ObservationPayload::Message(message)
                if message.mutation.operation == remuda_protocol::MutationOperation::Close =>
            {
                Some(message)
            }
            _ => None,
        })
        .expect("close frame");
    assert_eq!(close.status, ContentStatus::Interrupted);
    let ended = observed
        .iter()
        .find(|o| {
            matches!(
                &o.payload,
                ObservationPayload::Lifecycle(box_payload)
                    if matches!(box_payload.as_ref(),
                        remuda_protocol::LifecyclePayload::Native(n)
                            if n.native_name == "turn_ended")
            )
        })
        .unwrap();
    let ObservationPayload::Lifecycle(box_payload) = &ended.payload else {
        unreachable!()
    };
    let remuda_protocol::LifecyclePayload::Native(native) = box_payload.as_ref() else {
        unreachable!()
    };
    assert_eq!(
        native.related_ids.get("trigger").map(String::as_str),
        Some("ctrl_c")
    );
}

#[test]
fn a_hook_denied_tool_result_is_denied_not_failed() {
    let update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "hook-tool-1",
        "status": "failed",
        "error": "denied: SPIKE_DENIED",
        "rawOutput": null
    });
    assert_eq!(
        tool_outcome("failed", update.get("error"), None),
        ToolOutcome::Denied
    );
    assert_eq!(tool_outcome("failed", None, Some(1)), ToolOutcome::Failed);
    assert_eq!(
        tool_outcome("completed", None, Some(0)),
        ToolOutcome::Succeeded
    );
}

#[test]
fn permission_events_are_journaled_but_never_answered_from_files() {
    let data = json!({"type":"permission_requested","tool_name":"run_terminal_command"});
    let payload = permission_lifecycle("permission_requested", "waiting", &data).unwrap();
    let ObservationPayload::Lifecycle(box_payload) = payload else {
        panic!("expected lifecycle");
    };
    let remuda_protocol::LifecyclePayload::Native(native) = box_payload.as_ref() else {
        panic!("expected native");
    };
    assert_eq!(native.topic, LifecycleTopic::Permission);
    assert_eq!(
        native.related_ids.get("toolName").map(String::as_str),
        Some("run_terminal_command")
    );
}

#[test]
fn discovery_falls_back_from_pid_to_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let session = "sess-3";
    let encoded = crate::grok_session::encode_session_cwd(&cwd);
    let session_dir = dir.path().join("sessions").join(&encoded).join(session);
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("updates.jsonl"), "").unwrap();
    fs::write(session_dir.join("events.jsonl"), "").unwrap();
    fs::write(
        dir.path().join("active_sessions.json"),
        serde_json::to_string(&json!([
            {"session_id": session, "pid": 99, "cwd": cwd.to_string_lossy(), "opened_at": "t"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let mut adapter = GrokAdapter::new(AdapterHome {
        home: dir.path().to_path_buf(),
        cwd,
        pid: Some(42), // wrong pid; cwd must save discovery
    });
    assert!(adapter.poll().unwrap().is_empty());
    assert!(
        adapter.binding().is_some(),
        "cwd fallback binds the session"
    );
}
