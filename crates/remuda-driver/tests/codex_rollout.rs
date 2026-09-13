//! Synthetic edge cases complement the scrubbed real fixtures from the P6 spike.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use remuda_driver::codex_rollout::{
    CodexRolloutEvent as Event, RolloutTail, locate_rollout_in, parse_rollout_line,
    parse_rollout_line_at,
};
use serde_json::{Value, json};

/// Real Codex TUI output; provenance and scrubbing are documented alongside it.
const INTERACTIVE: &str = include_str!("fixtures/codex/interactive-0.154.0.jsonl");

#[test]
fn real_interactive_rollout_keeps_tool_turn_usage_and_interruption_evidence() {
    let mut calls = HashSet::new();
    let mut outputs = HashSet::new();
    let mut roles = HashSet::new();
    let mut completed = HashSet::new();
    let mut user_turns = HashMap::new();
    let mut aborted = None;
    let mut started = 0;
    let mut items = 0;
    let mut usage = 0;
    let mut token_counts = 0;
    for (ordinal, line) in INTERACTIVE.lines().enumerate() {
        let raw: Value = serde_json::from_str(line).unwrap();
        let record = parse_rollout_line(line).unwrap();
        assert_eq!(record.ordinal, Some(ordinal as u64));
        assert_eq!(record.timestamp.as_deref(), raw["timestamp"].as_str());
        assert!(record.timestamp.is_some());
        match record.event {
            Event::TaskStarted { turn_id, .. } => {
                assert!(turn_id.is_some());
                started += 1;
            }
            Event::TaskComplete { turn_id, error, .. } => {
                assert_eq!(error, None);
                completed.insert(turn_id.unwrap());
            }
            Event::ItemCompleted { turn_id, item } => {
                assert_eq!(item, raw["payload"]["item"]);
                if item["type"] == "UserMessage" {
                    let text = item["content"][0]["text"].as_str().unwrap().to_owned();
                    user_turns.insert(text, turn_id.unwrap());
                }
                items += 1;
            }
            Event::ToolCall {
                call_id,
                name,
                input,
            } => {
                assert_eq!(name.as_deref(), Some("exec_command"));
                assert_eq!(input, raw["payload"]["arguments"]);
                calls.insert(call_id.unwrap());
            }
            Event::ToolOutput { call_id, output } => {
                assert_eq!(output, raw["payload"]["output"]);
                outputs.insert(call_id.unwrap());
            }
            Event::Message { role, .. } => {
                roles.insert(role);
            }
            Event::TokenUsage { source_type, data } => {
                assert_eq!(data, raw["payload"]);
                if source_type == "token_usage_record" {
                    assert!(data["usage"]["input_tokens"].as_u64().is_some());
                    assert!(data["turn_token_usage"]["total_tokens"].as_u64().is_some());
                    usage += 1;
                } else {
                    assert_eq!(source_type, "token_count");
                    token_counts += 1;
                }
            }
            Event::Unknown { r#type } if r#type == "turn_aborted" => {
                aborted = Some(raw["payload"]["turn_id"].as_str().unwrap().to_owned());
            }
            _ => {}
        }
    }
    assert_eq!(
        (started, completed.len(), items, usage, token_counts),
        (6, 5, 22, 10, 10)
    );
    assert_eq!(calls.len(), 5);
    assert_eq!(calls, outputs);
    assert_eq!(
        roles,
        HashSet::from(["user".into(), "assistant".into(), "developer".into()])
    );
    assert!(!completed.contains(&aborted.expect("interrupted turn stayed Unknown")));
    // Native item_completed IDs provide this evidence, without assigning IDs
    // to response_item messages or inferring that every user message is a turn.
    assert_eq!(user_turns["STEER_BASE"], user_turns["STEER_FOLLOWUP"]);
    assert_ne!(user_turns["QUEUE_BASE"], user_turns["QUEUED_FOLLOWUP"]);
}

fn event(kind: &str, payload: Value) -> Event {
    parse_rollout_line(&json!({"type": kind, "payload": payload}).to_string())
        .expect("valid record")
        .event
}

#[test]
fn metadata_and_context_preserve_identity_and_effective_settings() {
    let meta = json!({"id":"thread-a", "session_id":"root-a", "cwd":"/home/dev/work",
        "cli_version":"0.154.0", "model_provider":"openai", "future_field": true});
    assert_eq!(
        event("session_meta", meta.clone()),
        Event::SessionMeta {
            session_id: Some("thread-a".into()),
            metadata: meta,
        }
    );
    let context = json!({"turn_id":"turn-a", "model":"fixture-model", "effort":"high",
        "approval_policy":"on-request", "sandbox_policy":{"type":"workspace-write"}});
    assert_eq!(
        event("turn_context", context.clone()),
        Event::TurnContext {
            turn_id: Some("turn-a".into()),
            model: Some("fixture-model".into()),
            effort: Some("high".into()),
            data: context,
        }
    );
}

#[test]
fn lifecycle_records_do_not_hide_completion_errors_or_item_fields() {
    assert_eq!(
        event(
            "event_msg",
            json!({"type":"task_started", "turn_id":"turn-a",
        "model_context_window": 120000})
        ),
        Event::TaskStarted {
            turn_id: Some("turn-a".into()),
            model_context_window: Some(120000),
        }
    );
    assert_eq!(
        event(
            "event_msg",
            json!({"type":"task_complete", "turn_id":"turn-a",
        "last_agent_message":"done", "error":{"message":"failed"}})
        ),
        Event::TaskComplete {
            turn_id: Some("turn-a".into()),
            last_agent_message: Some("done".into()),
            error: Some(json!({"message":"failed"})),
        }
    );
    let item = json!({"type":"AgentMessage", "id":"item-a", "content":[{"text":"done"}]});
    assert_eq!(
        event(
            "event_msg",
            json!({"type":"item_completed", "turn_id":"turn-a",
        "item":item})
        ),
        Event::ItemCompleted {
            turn_id: Some("turn-a".into()),
            item
        }
    );
    for kind in ["turn_aborted", "queued_message", "new_event_type"] {
        assert_eq!(
            event("event_msg", json!({"type":kind})),
            Event::Unknown {
                r#type: kind.into()
            }
        );
    }
}

#[test]
fn messages_and_reasoning_read_only_text_content() {
    assert_eq!(
        event(
            "response_item",
            json!({"type":"message", "role":"user", "content":[
        {"type":"input_text", "text":"first"}, {"type":"input_image", "image_url":"fixture"},
        {"type":"output_text", "text":"second"}]})
        ),
        Event::Message {
            role: "user".into(),
            text: "first\nsecond".into(),
        }
    );
    for (kind, role) in [("user_message", "user"), ("agent_message", "assistant")] {
        assert_eq!(
            event("event_msg", json!({"type":kind, "message":"text"})),
            Event::Message {
                role: role.into(),
                text: "text".into()
            }
        );
    }
    assert_eq!(
        event(
            "response_item",
            json!({"type":"reasoning", "summary":[
        {"type":"summary_text", "text":"summary"}], "encrypted_content":"opaque"})
        ),
        Event::Reasoning {
            text: "summary".into()
        }
    );
    assert_eq!(
        event(
            "response_item",
            json!({"type":"reasoning", "summary":[], "content":[
        {"type":"reasoning_text", "text":"visible"}]})
        ),
        Event::Reasoning {
            text: "visible".into()
        }
    );
    assert_eq!(
        event(
            "response_item",
            json!({"type":"reasoning", "encrypted_content":"opaque"})
        ),
        Event::Reasoning {
            text: String::new()
        }
    );
    assert_eq!(
        event(
            "event_msg",
            json!({"type":"agent_reasoning", "text":"reason"})
        ),
        Event::Reasoning {
            text: "reason".into()
        }
    );
}

#[test]
fn tool_calls_pair_by_call_id_without_coercing_input_or_output() {
    for (kind, key, input) in [
        (
            "function_call",
            "arguments",
            json!(r#"{"cmd":"printf test"}"#),
        ),
        ("custom_tool_call", "input", json!("freeform input")),
        (
            "function_call",
            "arguments",
            json!({"future":"structured input"}),
        ),
    ] {
        assert_eq!(
            event(
                "response_item",
                json!({"type":kind, "call_id":"call-a",
            "name":"fixture_tool", key:input})
            ),
            Event::ToolCall {
                call_id: Some("call-a".into()),
                name: Some("fixture_tool".into()),
                input,
            }
        );
    }
    assert_eq!(
        event(
            "response_item",
            json!({"type":"local_shell_call", "call_id":"call-a",
        "action":{"type":"exec", "command":["printf", "test"]}})
        ),
        Event::ToolCall {
            call_id: Some("call-a".into()),
            name: Some("local_shell".into()),
            input: json!({"type":"exec", "command":["printf", "test"]}),
        }
    );
    for (kind, output) in [
        ("function_call_output", json!("test")),
        (
            "custom_tool_call_output",
            json!([{"type":"input_text", "text":"test"}]),
        ),
    ] {
        assert_eq!(
            event(
                "response_item",
                json!({"type":kind, "call_id":"call-a", "output":output})
            ),
            Event::ToolOutput {
                call_id: Some("call-a".into()),
                output
            }
        );
    }
}

#[test]
fn usage_retains_cumulative_and_response_accounting_as_distinct_sources() {
    let data = json!({"type":"token_count", "info":{"total_token_usage":{"total_tokens":20},
        "last_token_usage":{"total_tokens":10}}, "rate_limits":{"primary":{"used_percent":1}}});
    assert_eq!(
        event("event_msg", data.clone()),
        Event::TokenUsage {
            source_type: "token_count".into(),
            data,
        }
    );
    let data = json!({"turn_id":"turn-a", "response_id":"response-a", "usage":{"input_tokens":5},
        "turn_token_usage":{"total_tokens":10}, "thread_token_usage":{"total_tokens":20}});
    assert_eq!(
        event("token_usage_record", data.clone()),
        Event::TokenUsage {
            source_type: "token_usage_record".into(),
            data,
        }
    );
}

#[test]
fn compaction_unknown_types_sparse_records_and_invalid_json() {
    assert_eq!(
        event("compacted", json!({"message":"summary"})),
        Event::Compacted
    );
    assert_eq!(
        event("event_msg", json!({"type":"context_compacted"})),
        Event::Compacted
    );
    assert_eq!(
        event("future_record", json!({})),
        Event::Unknown {
            r#type: "future_record".into()
        }
    );
    assert_eq!(
        event("response_item", json!({"type":"future_item"})),
        Event::Unknown {
            r#type: "future_item".into(),
        }
    );
    assert_eq!(
        event("event_msg", json!({"type":"task_complete"})),
        Event::TaskComplete {
            turn_id: None,
            last_agent_message: None,
            error: None,
        }
    );
    for line in [
        "{}",
        "null",
        "[]",
        r#"{"type":"event_msg","payload":false}"#,
    ] {
        assert!(matches!(
            parse_rollout_line(line).unwrap().event,
            Event::Unknown { .. }
        ));
    }
    assert!(parse_rollout_line("{truncated").is_err());
    let record =
        parse_rollout_line_at(r#"{"timestamp":"2026-09-14T00:00:00Z","ordinal":42}"#, 8).unwrap();
    assert_eq!(record.timestamp.as_deref(), Some("2026-09-14T00:00:00Z"));
    assert_eq!(record.ordinal, Some(42));
    assert_eq!(parse_rollout_line("{}").unwrap().ordinal, None);
    assert_eq!(parse_rollout_line_at("{}", 8).unwrap().ordinal, Some(8));
}

#[test]
fn tail_appends_complete_records_and_preserves_utf8_across_polls() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollout.jsonl");
    std::fs::write(&path, b"{\"type\":\"compacted\"}\r\n\n{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"\xe4").unwrap();
    let mut tail = RolloutTail::new(path.clone());
    let records = tail.poll().unwrap();
    assert_eq!(records, [r#"{"type":"compacted"}"#]);
    assert!(tail.poll().unwrap().is_empty());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"\xb8\xad\"}}\n").unwrap();
    let records = tail.poll().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        parse_rollout_line(&records[0]).unwrap().event,
        Event::Message {
            role: "user".into(),
            text: "中".into()
        }
    );
    assert_eq!(tail.path(), path);
    assert_eq!(tail.offset(), std::fs::metadata(&path).unwrap().len());
    assert!(tail.poll().unwrap().is_empty());
    std::fs::write(&path, b"{\"type\":\"compacted\"}\n").unwrap();
    assert_eq!(tail.poll().unwrap(), [r#"{"type":"compacted"}"#]);
}

fn write_meta(path: &Path, id: &str, root: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        json!({"type":"session_meta", "payload":{"id":id, "session_id":root}}).to_string() + "\n",
    )
    .unwrap();
}

#[test]
fn discovery_checks_metadata_and_prefers_active_before_archived() {
    let tmp = tempfile::tempdir().unwrap();
    let active = tmp
        .path()
        .join("sessions/2026/09/14/rollout-reverted-file-id.jsonl");
    let archived = tmp.path().join("archived_sessions/rollout-thread-a.jsonl");
    write_meta(&active, "thread-a", "root-a");
    write_meta(&archived, "thread-a", "root-a");
    write_meta(
        &tmp.path().join("sessions/rollout-thread-b.jsonl"),
        "thread-c",
        "root-a",
    );
    assert_eq!(
        locate_rollout_in(tmp.path(), "thread-a"),
        Some(active.clone())
    );
    for id in ["thread-b", "root-a", "", "../thread-a"] {
        assert_eq!(locate_rollout_in(tmp.path(), id), None);
    }
    std::fs::remove_file(active).unwrap();
    assert_eq!(locate_rollout_in(tmp.path(), "thread-a"), Some(archived));
}

#[test]
fn discovery_rejects_ambiguous_reverted_threads_without_archived_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let old = tmp.path().join("sessions/2026/09/13/rollout-old.jsonl");
    let new = tmp.path().join("sessions/2026/09/14/rollout-new.jsonl");
    let archived = tmp.path().join("archived_sessions/rollout-thread-a.jsonl");
    for path in [&old, &new, &archived] {
        write_meta(path, "thread-a", "root-a");
    }
    assert_eq!(locate_rollout_in(tmp.path(), "thread-a"), None);
    std::fs::remove_file(&old).unwrap();
    assert_eq!(locate_rollout_in(tmp.path(), "thread-a"), Some(new.clone()));
    std::fs::remove_file(new).unwrap();
    assert_eq!(locate_rollout_in(tmp.path(), "thread-a"), Some(archived));
    write_meta(
        &tmp.path().join("archived_sessions/rollout-reverted.jsonl"),
        "thread-a",
        "root-a",
    );
    assert_eq!(locate_rollout_in(tmp.path(), "thread-a"), None);
}

#[test]
fn discovery_requires_thread_id_even_when_parser_accepts_a_root_only_header() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let line = json!({"type":"session_meta", "payload":{"session_id":"root-a"}}).to_string();
    std::fs::write(sessions.join("rollout-root-a.jsonl"), line.clone() + "\n").unwrap();
    assert!(matches!(parse_rollout_line(&line).unwrap().event,
        Event::SessionMeta { session_id: Some(id), .. } if id == "root-a"));
    assert_eq!(locate_rollout_in(tmp.path(), "root-a"), None);
}

#[test]
#[cfg(unix)]
fn discovery_does_not_follow_file_or_directory_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let rollout = outside.path().join("rollout-thread-a.jsonl");
    write_meta(&rollout, "thread-a", "thread-a");
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::os::unix::fs::symlink(&rollout, sessions.join("rollout-thread-a.jsonl")).unwrap();
    std::os::unix::fs::symlink(outside.path(), sessions.join("linked-dir")).unwrap();
    assert_eq!(locate_rollout_in(tmp.path(), "thread-a"), None);
    std::fs::remove_dir_all(&sessions).unwrap();
    std::os::unix::fs::symlink(outside.path(), sessions).unwrap();
    assert_eq!(locate_rollout_in(tmp.path(), "thread-a"), None);
}
