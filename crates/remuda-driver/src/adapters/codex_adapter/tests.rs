//! Tests against the captured real 0.154.0 session and synthetic records.

use super::*;
use remuda_protocol::ObservationPayload;
use serde_json::json;
use std::io::Write;
use std::path::Path;

const REAL_FIXTURE: &str = include_str!("../../../tests/fixtures/codex/interactive-0.154.0.jsonl");

fn adapter_with_rollout(dir: &Path, session_id: &str) -> CodexAdapter {
    let date = "2026/09/14";
    let session_dir = dir.join("sessions").join(date);
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join(format!("rollout-2026-09-14T02-21-06-{session_id}.jsonl"));
    // The locator verifies the file's session_meta.id before binding.
    std::fs::write(
        &path,
        format!(
            "{{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"session_id\":\"{session_id}\"}}}}\n"
        ),
    )
    .unwrap();
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: dir.to_path_buf(),
        cwd: dir.to_path_buf(),
        pid: None,
    });
    adapter.confirm_session(session_id);
    assert!(adapter.discover().unwrap());
    assert!(adapter.binding().is_some());
    adapter
}

fn feed(adapter: &mut CodexAdapter, lines: &str) -> Vec<AdapterObservation> {
    let mut out = Vec::new();
    for (ordinal, line) in lines
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        out.extend(adapter.on_line(ordinal as u64, line));
    }
    out
}

#[test]
fn the_real_fixture_produces_the_measured_turn_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let session = "01a09c00-64d4-7ce1-8537-984e896b8e8a";
    let mut adapter = adapter_with_rollout(dir.path(), session);
    // Copy the real rollout into the shadow home the adapter discovered.
    let path = adapter.binding().unwrap().main_file.clone().unwrap();
    std::fs::write(&path, REAL_FIXTURE).unwrap();
    let observations = adapter.poll().unwrap();
    let names: Vec<String> = observations
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
        .collect();
    // 6 task_started, 5 task_complete, 1 turn_aborted (evidence §A3).
    assert_eq!(
        names
            .iter()
            .filter(|name| name.as_str() == "task_started")
            .count(),
        6
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| name.as_str() == "task_complete")
            .count(),
        5
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| name.as_str() == "turn_aborted")
            .count(),
        1
    );
    // Every boundary is a file-channel structured fact.
    assert!(
        observations
            .iter()
            .all(|observed| observed.channel == remuda_protocol::SourceChannel::File)
    );
}

#[test]
fn interrupted_turn_comes_back_idle_not_failed() {
    let lines = r#"
{"ordinal":1,"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}
{"ordinal":2,"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t1","reason":"interrupted"}}
"#;
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: tempfile::tempdir().unwrap().path().to_path_buf(),
        cwd: std::path::PathBuf::from("/w"),
        pid: None,
    });
    let observed = feed(&mut adapter, lines);
    let aborted = observed
        .iter()
        .find(|o| {
            matches!(
                &o.payload,
                ObservationPayload::Lifecycle(box_payload)
                    if matches!(box_payload.as_ref(),
                        remuda_protocol::LifecyclePayload::Native(n)
                            if n.native_name == "turn_aborted")
            )
        })
        .expect("turn_aborted lifecycle");
    let ObservationPayload::Lifecycle(box_payload) = &aborted.payload else {
        panic!("expected lifecycle");
    };
    let remuda_protocol::LifecyclePayload::Native(native) = box_payload.as_ref() else {
        panic!("expected native");
    };
    assert_eq!(
        native.status,
        remuda_protocol::Knowledge::Known {
            value: "idle".into()
        }
    );
    assert_ne!(native.severity, remuda_protocol::Severity::Error);
}

#[test]
fn a_foreign_abort_reason_does_not_end_the_turn() {
    let lines = r#"
{"ordinal":1,"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t1","reason":"context_compacted"}}
"#;
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: tempfile::tempdir().unwrap().path().to_path_buf(),
        cwd: std::path::PathBuf::from("/w"),
        pid: None,
    });
    let observed = feed(&mut adapter, lines);
    assert!(
        observed.is_empty(),
        "an unknown abort reason is not an interrupt"
    );
}

#[test]
fn completed_items_map_messages_and_a_late_tool_result_keeps_the_old_turn() {
    let lines = r#"
{"ordinal":1,"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}
{"ordinal":2,"type":"response_item","payload":{"type":"function_call","id":"fc_1","call_id":"c1","name":"exec_command","arguments":"{\"cmd\":\"sleep 20\"}","internal_chat_message_metadata_passthrough":{"turn_id":"t1"}}}
{"ordinal":3,"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t1","reason":"interrupted"}}
{"ordinal":4,"type":"event_msg","payload":{"type":"task_started","turn_id":"t2"}}
{"ordinal":5,"type":"event_msg","payload":{"type":"item_completed","turn_id":"t1","item":{"type":"CommandExecution","id":"c1","command":[{"command":"sleep 20","arguments":[]}],"cwd":"/w","status":"completed","aggregated_output":"LATE","exit_code":0}}}
"#;
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: tempfile::tempdir().unwrap().path().to_path_buf(),
        cwd: std::path::PathBuf::from("/w"),
        pid: None,
    });
    let observed = feed(&mut adapter, lines);
    // The late result carries the OLD turn id t1 (evidence ordinal 86).
    let late = observed
        .iter()
        .find(|o| matches!(o.payload, ObservationPayload::ToolResult(_)))
        .expect("late tool result");
    assert_eq!(late.turn_id.as_deref(), Some("t1"));
    let ObservationPayload::ToolResult(result) = &late.payload else {
        unreachable!()
    };
    assert_eq!(result.outcome, ToolOutcome::Succeeded);
}

#[test]
fn a_hook_rejected_command_is_denied_without_an_exit_code() {
    let item = json!({
        "type": "CommandExecution",
        "id": "spike-call-1",
        "status": {"type": "error", "message": "CreateProcess: Rejected(\"spike decision deny\")"},
        "exit_code": null
    });
    let (outcome, exit_code) = command_outcome(&item);
    assert_eq!(outcome, ToolOutcome::Denied);
    assert_eq!(exit_code, None);
}

#[test]
fn duplicate_representations_do_not_double_journal() {
    let lines = r#"
{"ordinal":1,"type":"event_msg","payload":{"type":"item_completed","turn_id":"t1","item":{"type":"AgentMessage","id":"msg_x","content":[{"type":"text","text":"ANSWER"}]}}}
{"ordinal":2,"type":"response_item","payload":{"type":"message","id":"msg_x","role":"assistant","content":[{"type":"output_text","text":"ANSWER"}]}}
{"ordinal":3,"type":"event_msg","payload":{"type":"item_completed","turn_id":"t1","item":{"type":"AgentMessage","id":"msg_x","content":[{"type":"text","text":"ANSWER"}]}}}
"#;
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: tempfile::tempdir().unwrap().path().to_path_buf(),
        cwd: std::path::PathBuf::from("/w"),
        pid: None,
    });
    let observed = feed(&mut adapter, lines);
    let messages = observed
        .iter()
        .filter(|o| matches!(o.payload, ObservationPayload::Message(_)))
        .count();
    assert_eq!(
        messages, 1,
        "completed item once; response_item duplicate ignored"
    );
}

#[test]
fn usage_events_produce_turn_and_session_snapshots_at_turn_end() {
    let lines = r#"
{"ordinal":1,"type":"turn_context","payload":{"turn_id":"t1","model":"gpt-5.4"}}
{"ordinal":2,"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}
{"ordinal":3,"type":"token_usage_record","payload":{"turn_id":"t1","response_id":"r1","usage":{"input_tokens":100,"cached_input_tokens":10,"cache_write_input_tokens":0,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120}}}
{"ordinal":4,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":10,"cache_write_input_tokens":0,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120}}}}
{"ordinal":5,"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}
"#;
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: tempfile::tempdir().unwrap().path().to_path_buf(),
        cwd: std::path::PathBuf::from("/w"),
        pid: None,
    });
    let observed = feed(&mut adapter, lines);
    let usages: Vec<_> = observed
        .iter()
        .filter_map(|o| match &o.payload {
            ObservationPayload::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect();
    // One turn snapshot + one session snapshot.
    assert_eq!(usages.len(), 2);
    assert_eq!(usages[0].scope, remuda_protocol::UsageScope::Turn);
    assert_eq!(usages[1].scope, remuda_protocol::UsageScope::Session);
    // Both estimated; cumulative token_count did not double the tokens.
    for payload in &usages {
        assert_eq!(payload.accounting, remuda_protocol::Accounting::Estimated);
    }
    // Total spans every bucket: 90 uncached input + 10 cache read + 20 output
    // (the same definition usage::tests locks in).
    assert_eq!(
        usages[1].total_tokens,
        remuda_protocol::Knowledge::Known {
            value: remuda_protocol::U64(120)
        }
    );
}

#[test]
fn discovery_refuses_to_invent_a_session_when_the_index_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: dir.path().to_path_buf(),
        cwd: dir.path().to_path_buf(),
        pid: None,
    });
    assert!(adapter.poll().unwrap().is_empty());
    assert!(adapter.binding().is_none());
}

#[test]
fn a_newline_terminated_partial_line_is_held_until_complete() {
    let dir = tempfile::tempdir().unwrap();
    let session = "01aa0000-0000-7000-0000-000000000001";
    let session_dir = dir.path().join("sessions/2026/09/14");
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join(format!("rollout-2026-09-14T00-00-00-{session}.jsonl"));
    {
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\",\"session_id\":\"{session}\"}}}}"
        )
        .unwrap();
        // Half a line, no newline yet.
        write!(file, "{{\"ordinal\":1").unwrap();
    }
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: dir.path().to_path_buf(),
        cwd: dir.path().to_path_buf(),
        pid: None,
    });
    adapter.confirm_session(session);
    assert!(adapter.poll().unwrap().is_empty(), "partial line held");
    // Complete it.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"t1\"}}\n")
        .unwrap();
    let observed = adapter.poll().unwrap();
    assert!(observed.iter().any(|o| matches!(
        &o.payload,
        ObservationPayload::Lifecycle(box_payload)
            if matches!(box_payload.as_ref(),
                remuda_protocol::LifecyclePayload::Native(n)
                    if n.native_name == "task_started")
    )));
}
