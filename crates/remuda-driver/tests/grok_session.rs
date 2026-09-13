//! Synthetic boundary cases complement the scrubbed real native-session files.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use remuda_driver::grok_session::{
    GrokSessionEvent as Event, GrokSessionUpdate as Update, SessionSelector, SessionTail,
    encode_session_cwd, locate_session, parse_active_sessions, parse_event_line, parse_update_line,
    read_active_sessions,
};
use serde_json::{Value, json};

/// Real Grok TUI captures; provenance and scrubbing live in the fixture README.
const REAL_UPDATES: &str = include_str!("fixtures/grok/tui-updates.jsonl");
const REAL_EVENTS: &str = include_str!("fixtures/grok/tui-events.jsonl");
const REAL_ACTIVE: &str = include_str!("fixtures/grok/active-sessions.json");

#[test]
fn real_updates_cover_all_requested_types_and_keep_tool_correlation() {
    let mut kinds = HashSet::new();
    let mut calls = HashSet::new();
    let mut results = HashSet::new();
    for line in REAL_UPDATES.lines() {
        let parsed = parse_update_line(line).expect("real update");
        assert!(parsed.timestamp.is_some());
        assert!(parsed.session_id.is_some());
        kinds.insert(match parsed.update {
            Update::AgentMessageChunk { .. } => "agent_message_chunk",
            Update::AgentThoughtChunk { .. } => "agent_thought_chunk",
            Update::UserMessageChunk { .. } => "user_message_chunk",
            Update::ToolCall { tool_call_id } => {
                calls.insert(tool_call_id.expect("native call id"));
                "tool_call"
            }
            Update::ToolCallUpdate { tool_call_id } => {
                results.insert(tool_call_id.expect("native result id"));
                "tool_call_update"
            }
            Update::HookExecution => "hook_execution",
            Update::Unknown { .. } => "unknown",
        });
        assert_eq!(
            parsed.frame,
            serde_json::from_str::<Value>(line).expect("source JSON")
        );
    }
    assert_eq!(
        kinds,
        HashSet::from([
            "agent_message_chunk",
            "agent_thought_chunk",
            "user_message_chunk",
            "tool_call",
            "tool_call_update",
            "hook_execution",
            "unknown"
        ])
    );
    assert!(!calls.is_empty());
    assert!(results.is_subset(&calls));
}

#[test]
fn real_events_keep_lifecycle_and_terminal_outcome_without_synthesizing_completion() {
    let mut kinds = HashSet::new();
    let mut terminal_outcomes = Vec::new();
    for line in REAL_EVENTS.lines() {
        let parsed = parse_event_line(line).expect("real event");
        assert!(parsed.timestamp.is_some());
        kinds.insert(match parsed.event {
            Event::TurnStarted { .. } => "turn_started",
            Event::PhaseChanged { .. } => "phase_changed",
            Event::FirstToken => "first_token",
            Event::Unknown { ref kind } => {
                if kind == "turn_ended" {
                    terminal_outcomes.push(parsed.data["outcome"].clone());
                }
                "unknown"
            }
        });
    }
    assert_eq!(
        kinds,
        HashSet::from(["turn_started", "phase_changed", "first_token", "unknown"])
    );
    assert!(terminal_outcomes.contains(&json!("completed")));
}

#[test]
fn real_active_registry_snapshot_decodes_required_identity() {
    let active = parse_active_sessions(REAL_ACTIVE).expect("real snapshot");
    assert!(!active.is_empty());
    for session in active {
        assert!(session.pid > 0);
        assert!(!session.session_id.is_empty());
        assert!(session.cwd.is_absolute());
        assert!(!session.opened_at.is_empty());
    }
}

fn frame(update: Value) -> String {
    json!({"timestamp":42,"method":"session/update","params":{
        "sessionId":"session-a", "update":update,"_meta":{"eventId":"native-7"}}})
    .to_string()
}

#[test]
fn all_message_chunk_types_keep_content_and_native_metadata() {
    let content = json!({"type":"text","text":"一段 streaming text","future":true});
    for (kind, expected) in [
        (
            "agent_message_chunk",
            Update::AgentMessageChunk {
                content: content.clone(),
            },
        ),
        (
            "agent_thought_chunk",
            Update::AgentThoughtChunk {
                content: content.clone(),
            },
        ),
        (
            "user_message_chunk",
            Update::UserMessageChunk {
                content: content.clone(),
            },
        ),
    ] {
        let record = parse_update_line(&frame(json!({"sessionUpdate":kind,"content":content})))
            .expect("chunk");
        assert_eq!(record.timestamp, Some(42));
        assert_eq!(record.session_id.as_deref(), Some("session-a"));
        assert_eq!(record.update, expected);
        assert_eq!(record.frame["params"]["_meta"]["eventId"], "native-7");
    }
    let image = json!({"type":"image","data":"fixture-image","mimeType":"image/png"});
    assert_eq!(
        parse_update_line(&frame(
            json!({"sessionUpdate":"user_message_chunk","content":image})
        ))
        .expect("non-text")
        .update,
        Update::UserMessageChunk { content: image }
    );
}

#[test]
fn tool_records_keep_native_ids_inputs_status_and_outputs() {
    for (kind, expected) in [
        (
            "tool_call",
            Update::ToolCall {
                tool_call_id: Some("call-9".into()),
            },
        ),
        (
            "tool_call_update",
            Update::ToolCallUpdate {
                tool_call_id: Some("call-9".into()),
            },
        ),
    ] {
        let update = json!({"sessionUpdate":kind,"toolCallId":"call-9","title":"Bash",
            "status":"completed","rawInput":{"command":"printf fixture"},
            "content":[{"type":"content","content":{"type":"text","text":"fixture"}}]});
        let parsed = parse_update_line(&frame(update.clone())).expect("tool");
        assert_eq!(parsed.update, expected);
        assert_eq!(parsed.frame["params"]["update"], update);
    }
    assert_eq!(
        parse_update_line(&frame(json!({"sessionUpdate":"tool_call","id":"wrong-id"})))
            .expect("sparse tool")
            .update,
        Update::ToolCall { tool_call_id: None }
    );
}

#[test]
fn hook_and_unknown_updates_keep_their_native_frames() {
    assert_eq!(
        parse_update_line(&frame(
            json!({"sessionUpdate":"hook_execution","event":"Stop"})
        ))
        .expect("hook")
        .update,
        Update::HookExecution
    );
    for kind in ["plan", "queue_changed", "future_update"] {
        let parsed =
            parse_update_line(&frame(json!({"sessionUpdate":kind,"future":42}))).expect("unknown");
        assert_eq!(parsed.update, Update::Unknown { kind: kind.into() });
        assert_eq!(parsed.frame["params"]["update"]["future"], 42);
    }
    let extension = json!({"method":"_x.ai/session/update","params":{"update":{
        "sessionUpdate":"turn_completed","stop_reason":"end_turn"}}});
    let parsed = parse_update_line(&extension.to_string()).expect("extension");
    assert_eq!(
        parsed.update,
        Update::Unknown {
            kind: "turn_completed".into()
        }
    );
    assert_eq!(parsed.session_id, None);
    assert_eq!(parsed.timestamp, None);
    assert_eq!(parsed.frame, extension);
    let permission = json!({"method":"session/request_permission","id":3,
        "params":{"sessionId":"native","options":[]}});
    let parsed = parse_update_line(&permission.to_string()).expect("permission stays unknown");
    assert_eq!(
        parsed.update,
        Update::Unknown {
            kind: "session/request_permission".into()
        }
    );
    assert_eq!(parsed.frame, permission);
}

#[test]
fn native_events_preserve_types_optional_fields_and_unrecognized_completion() {
    let started = json!({"ts":"2026-09-13T18:56:48.710Z","type":"turn_started",
        "session_id":"native","turn_number":0,"model_id":"fixture","yolo_mode":false});
    let parsed = parse_event_line(&started.to_string()).expect("started");
    assert_eq!(
        parsed.event,
        Event::TurnStarted {
            turn_number: Some(0),
            model_id: Some("fixture".into()),
            yolo_mode: Some(false)
        }
    );
    assert_eq!(parsed.session_id.as_deref(), Some("native"));
    assert_eq!(
        parsed.timestamp.as_deref(),
        Some("2026-09-13T18:56:48.710Z")
    );
    assert_eq!(parsed.data, started);
    assert_eq!(
        parse_event_line(r#"{"type":"phase_changed","phase":"waiting_for_model"}"#)
            .expect("phase")
            .event,
        Event::PhaseChanged {
            phase: Some("waiting_for_model".into())
        }
    );
    let first = parse_event_line(r#"{"type":"first_token"}"#).expect("first");
    assert_eq!(first.event, Event::FirstToken);
    assert_eq!(first.timestamp, None);
    assert_eq!(first.session_id, None);
    assert_eq!(
        parse_event_line(r#"{"type":"turn_started"}"#)
            .expect("sparse")
            .event,
        Event::TurnStarted {
            turn_number: None,
            model_id: None,
            yolo_mode: None
        }
    );
    for kind in [
        "turn_ended",
        "turn_complete",
        "queue_changed",
        "future_event",
    ] {
        let data = json!({"type":kind,"outcome":"interrupted"});
        let parsed = parse_event_line(&data.to_string()).expect("unknown event");
        assert_eq!(parsed.event, Event::Unknown { kind: kind.into() });
        assert_eq!(parsed.data, data);
    }
}

#[test]
fn malformed_records_do_not_become_semantic_events() {
    for line in [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"method":"session/update","params":{}}"#,
        r#"{"method":"session/update","params":{"update":{"sessionUpdate":3}}}"#,
    ] {
        assert!(parse_update_line(line).is_err(), "{line}");
    }
    for line in ["{", "null", "[]", "{}", r#"{"type":3}"#] {
        assert!(parse_event_line(line).is_err(), "{line}");
    }
}

fn append(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("append")
        .write_all(bytes)
        .expect("write");
}

#[test]
fn tail_tracks_bytes_and_waits_for_partial_utf8_and_newline() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("updates.jsonl");
    let whole = frame(json!({"sessionUpdate":"agent_message_chunk",
        "content":{"type":"text","text":"中文"}}));
    let split = whole.find('中').expect("unicode") + 1;
    std::fs::write(&path, &whole.as_bytes()[..split]).expect("write");
    let mut tail = SessionTail::new(path.clone());
    assert_eq!(tail.path(), path);
    assert!(tail.poll().expect("partial").is_empty());
    assert_eq!(tail.offset(), split as u64);
    append(&path, &whole.as_bytes()[split..]);
    assert!(tail.poll().expect("needs newline").is_empty());
    append(&path, b"\r\n\n{malformed}\n");
    let lines = tail.poll().expect("whole");
    assert_eq!(lines, vec![whole, "{malformed}".into()]);
    assert!(parse_update_line(&lines[0]).is_ok());
    assert!(parse_update_line(&lines[1]).is_err());
    assert_eq!(
        tail.offset(),
        std::fs::metadata(&path).expect("metadata").len()
    );
    assert!(tail.poll().expect("no repeats").is_empty());
}

#[test]
fn tail_resets_after_truncation_and_can_continue_after_bad_json() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("events.jsonl");
    std::fs::write(
        &path,
        "{broken}\n{\"type\":\"first_token\"}\n{\"old partial",
    )
    .expect("write");
    let mut tail = SessionTail::new(path.clone());
    let lines = tail.poll().expect("poll");
    assert!(parse_event_line(&lines[0]).is_err());
    assert_eq!(
        parse_event_line(&lines[1])
            .expect("valid after error")
            .event,
        Event::FirstToken
    );
    std::fs::write(&path, "{\"type\":\"first_token\"}\n").expect("truncate");
    assert_eq!(
        tail.poll().expect("reset"),
        vec!["{\"type\":\"first_token\"}"]
    );
    std::fs::remove_file(&path).expect("remove");
    assert_eq!(
        tail.poll().expect_err("missing").kind(),
        std::io::ErrorKind::NotFound
    );
}

fn entry(id: &str, pid: u32, cwd: &Path) -> Value {
    json!({"session_id":id,"pid":pid,"cwd":cwd,"opened_at":"2026-09-13T18:56:35Z"})
}

fn registry(home: &Path, entries: &[Value]) {
    std::fs::write(
        home.join("active_sessions.json"),
        serde_json::to_vec(entries).expect("encode"),
    )
    .expect("registry");
}

#[test]
fn discovery_requires_unique_registry_identity_and_existing_directory() {
    let tmp = tempfile::tempdir().expect("tmp");
    let cwd = Path::new("/tmp/fixture work");
    let dir = tmp
        .path()
        .join("sessions/%2Ftmp%2Ffixture%20work/session-a");
    registry(tmp.path(), &[entry("session-a", 123, cwd)]);
    assert!(
        locate_session(tmp.path(), SessionSelector::Pid(123))
            .expect("missing files")
            .is_none()
    );
    std::fs::create_dir_all(&dir).expect("mkdir");
    for selector in [SessionSelector::Pid(123), SessionSelector::Cwd(cwd)] {
        let located = locate_session(tmp.path(), selector)
            .expect("lookup")
            .expect("unique");
        assert_eq!(located.directory, dir);
        assert_eq!(located.session.session_id, "session-a");
    }
    assert!(
        locate_session(tmp.path(), SessionSelector::Pid(999))
            .expect("absent")
            .is_none()
    );
    registry(
        tmp.path(),
        &[entry("session-a", 123, cwd), entry("session-b", 456, cwd)],
    );
    assert!(
        locate_session(tmp.path(), SessionSelector::Cwd(cwd))
            .expect("ambiguous cwd")
            .is_none()
    );
    registry(
        tmp.path(),
        &[
            entry("session-a", 123, cwd),
            entry("session-b", 123, Path::new("/other")),
        ],
    );
    assert!(
        locate_session(tmp.path(), SessionSelector::Pid(123))
            .expect("ambiguous pid")
            .is_none()
    );
}

#[test]
fn malformed_registry_fails_closed_instead_of_discarding_ambiguous_entries() {
    for input in [
        "{}",
        "null",
        "[{}]",
        r#"[{"session_id":"../escape","pid":1,"cwd":"/work","opened_at":"x"}]"#,
        r#"[{"session_id":"s","pid":0,"cwd":"/work","opened_at":"x"}]"#,
        r#"[{"session_id":"s","pid":1,"cwd":"relative","opened_at":"x"}]"#,
    ] {
        assert!(parse_active_sessions(input).is_err(), "{input}");
    }
    let tmp = tempfile::tempdir().expect("tmp");
    registry(
        tmp.path(),
        &[entry("valid", 12, Path::new("/work")), json!({"pid":12})],
    );
    assert_eq!(
        read_active_sessions(tmp.path())
            .expect_err("bad record")
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    assert!(locate_session(tmp.path(), SessionSelector::Pid(12)).is_err());
    assert!(
        parse_active_sessions("[]")
            .expect("empty valid registry")
            .is_empty()
    );
}

#[test]
fn cwd_encoding_matches_native_percent_encoded_path_and_utf8() {
    assert_eq!(
        encode_session_cwd(Path::new("/private/tmp/remuda-grokspike/tui/work")),
        "%2Fprivate%2Ftmp%2Fremuda-grokspike%2Ftui%2Fwork"
    );
    assert_eq!(
        encode_session_cwd(Path::new("/work/a b+%~_-.中")),
        "%2Fwork%2Fa%20b%2B%25~_-.%E4%B8%AD"
    );
}

#[cfg(unix)]
#[test]
fn discovery_resolves_cwd_aliases_but_rejects_session_directory_symlinks() {
    let tmp = tempfile::tempdir().expect("tmp");
    let cwd = tmp.path().join("work");
    std::fs::create_dir(&cwd).expect("work");
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&cwd, &alias).expect("cwd alias");
    let canonical = std::fs::canonicalize(&cwd).expect("canonical cwd");
    registry(tmp.path(), &[entry("session-a", 123, &canonical)]);
    let encoded_dir = tmp
        .path()
        .join("sessions")
        .join(encode_session_cwd(&canonical));
    std::fs::create_dir_all(encoded_dir.join("session-a")).expect("session");
    assert!(
        locate_session(tmp.path(), SessionSelector::Cwd(&alias))
            .expect("alias")
            .is_some()
    );
    std::fs::remove_dir(encoded_dir.join("session-a")).expect("remove");
    std::os::unix::fs::symlink(&cwd, encoded_dir.join("session-a")).expect("session symlink");
    assert!(
        locate_session(tmp.path(), SessionSelector::Pid(123))
            .expect("reject symlink")
            .is_none()
    );
}
