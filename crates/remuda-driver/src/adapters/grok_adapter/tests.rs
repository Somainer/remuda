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

/// Bind an adapter directly to a session dir holding one updates file. The
/// frames are synthetic test input, not captured frames.
fn adapter_for_updates(updates: &str) -> (tempfile::TempDir, GrokAdapter) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    let session = "sess-tools";
    let encoded = crate::grok_session::encode_session_cwd(&cwd);
    let session_dir = dir.path().join("sessions").join(&encoded).join(session);
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("updates.jsonl"), updates).unwrap();
    fs::write(session_dir.join("events.jsonl"), "").unwrap();
    fs::write(session_dir.join("usage.json"), "{}\n").unwrap();
    let mut adapter = GrokAdapter::new(AdapterHome {
        home: dir.path().to_path_buf(),
        cwd,
        pid: Some(4242),
    });
    adapter.bind_session_dir(session, session_dir);
    (dir, adapter)
}

fn frame(update: Value) -> String {
    let wrapped = json!({
        "method": "session/update",
        "params": { "sessionId": "sess-tools", "update": update },
    });
    let mut line = serde_json::to_string(&wrapped).unwrap();
    line.push('\n');
    line
}

fn tool_calls(observed: &[AdapterObservation]) -> Vec<&remuda_protocol::ToolCallPayload> {
    observed
        .iter()
        .filter_map(|o| match &o.payload {
            ObservationPayload::ToolCall(call) => Some(call.as_ref()),
            _ => None,
        })
        .collect()
}

fn tool_results(observed: &[AdapterObservation]) -> Vec<&remuda_protocol::ToolResultPayload> {
    observed
        .iter()
        .filter_map(|o| match &o.payload {
            ObservationPayload::ToolResult(result) => Some(result.as_ref()),
            _ => None,
        })
        .collect()
}

#[test]
fn tool_identity_comes_from_meta_not_title_synthesized() {
    // Synthesized from docs, not captured [U]: the 1.0.30 fixture always has
    // `_meta`, so the title/name split is asserted with constructed frames.
    let updates = [
        frame(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-synth-1",
            "title": "Execute `echo hi`",
            "rawInput": { "command": "echo hi" },
            "_meta": { "x.ai/tool": { "name": "run_terminal_command", "kind": "execute" } }
        })),
        // A legacy/edge frame with no `_meta`: the name stays unknown rather
        // than being faked from the display title (protocol §5.7).
        frame(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-synth-2",
            "title": "Some human sentence",
            "rawInput": {}
        })),
    ]
    .concat();
    let (_dir, mut adapter) = adapter_for_updates(&updates);
    let observed = adapter.poll().unwrap();
    let calls = tool_calls(&observed);
    assert_eq!(calls.len(), 2);

    assert_eq!(
        calls[0].tool_name,
        remuda_protocol::Knowledge::Known {
            value: "run_terminal_command".into()
        }
    );
    assert_eq!(
        calls[0].display_title,
        remuda_protocol::Knowledge::Known {
            value: "Execute `echo hi`".into()
        }
    );
    assert_eq!(calls[0].category, remuda_protocol::ToolCategory::Shell);

    match &calls[1].tool_name {
        remuda_protocol::Knowledge::Unknown { reason, .. } => assert_eq!(reason, "not-emitted"),
        other => panic!("expected unknown tool name, got {other:?}"),
    }
    assert_eq!(
        calls[1].display_title,
        remuda_protocol::Knowledge::Known {
            value: "Some human sentence".into()
        }
    );
}

#[test]
fn the_real_fixtures_statusless_update_is_a_running_replace() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let mut adapter = write_real_session(
        dir.path(),
        &cwd,
        24069,
        "01a09c24-46ef-7a03-9c89-88f1bc00bd0c",
    );
    let observed = adapter.poll().unwrap();
    let call_id = "call-spike-1789326032369250000";
    let mine: Vec<_> = observed
        .iter()
        .filter(|o| o.item_id.as_deref() == Some(call_id))
        .collect();
    // Pending tool_call, statusless progress, terminal result.
    assert_eq!(mine.len(), 3, "one call observation set: {mine:?}");

    let proposed = match &mine[0].payload {
        ObservationPayload::ToolCall(call) => call,
        other => panic!("expected tool call, got {other:?}"),
    };
    assert_eq!(proposed.state, remuda_protocol::ToolCallState::Proposed);
    assert_eq!(proposed.mutation.revision, remuda_protocol::U64(1));
    assert_eq!(
        proposed.mutation.operation,
        remuda_protocol::MutationOperation::Open
    );
    assert_eq!(proposed.category, remuda_protocol::ToolCategory::Shell);
    let node = proposed.tool_call_id.clone();

    let running = match &mine[1].payload {
        ObservationPayload::ToolCall(call) => call,
        other => panic!("expected running call, got {other:?}"),
    };
    assert_eq!(running.state, remuda_protocol::ToolCallState::Running);
    assert_eq!(running.mutation.revision, remuda_protocol::U64(2));
    assert_eq!(
        running.mutation.operation,
        remuda_protocol::MutationOperation::Replace
    );
    assert_eq!(
        running.mutation.base_revision,
        Some(remuda_protocol::U64(1))
    );
    assert_eq!(running.tool_call_id, node, "same node as the proposal");
    // The human title arrives on the progress frame; the stable name does not
    // change.
    assert_eq!(
        running.display_title,
        remuda_protocol::Knowledge::Known {
            value: "Execute `printf SPIKE_TOOL_OK > spike-result.txt`".into()
        }
    );
    assert_eq!(
        running.tool_name,
        remuda_protocol::Knowledge::Known {
            value: "run_terminal_command".into()
        }
    );
    let running_input = match &running.input {
        remuda_protocol::Knowledge::Known { value } => value,
        other => panic!("input refreshed on running: {other:?}"),
    };
    assert_eq!(running_input["variant"], json!("Bash"));

    let result = match &mine[2].payload {
        ObservationPayload::ToolResult(result) => result,
        other => panic!("expected tool result, got {other:?}"),
    };
    // The closing revision is strictly greater than the Running one, so the
    // timeline accepts it instead of freezing the card on "proposed".
    assert_eq!(result.mutation.revision, remuda_protocol::U64(3));
    assert_eq!(
        result.mutation.operation,
        remuda_protocol::MutationOperation::Close
    );
    assert_eq!(result.mutation.base_revision, Some(remuda_protocol::U64(2)));
    assert_eq!(result.stage, remuda_protocol::ResultStage::Final);
    assert_eq!(result.outcome, ToolOutcome::Succeeded);
    assert_eq!(
        result.exit_code,
        remuda_protocol::Knowledge::Known { value: 0 }
    );
    assert!(
        result.blocks.iter().any(
            |block| matches!(block, remuda_protocol::ContentBlock::Text(text)
                if text.text == "exit: 0\n")
        ),
        "output_for_prompt survives as the result text: {:?}",
        result.blocks
    );

    // The ask_user_question call goes through the same revisions with the
    // Other category.
    let question_running = observed.iter().any(|o| {
        o.item_id.as_deref() == Some("call-spike-1789326112818929000")
            && matches!(&o.payload, ObservationPayload::ToolCall(call)
                if call.state == remuda_protocol::ToolCallState::Running
                    && call.category == remuda_protocol::ToolCategory::Other)
    });
    assert!(question_running, "question call reached Running/Other");
}

#[test]
fn category_resolves_by_name_then_kind_then_other() {
    // Name table (design doc §3.1).
    let by_name = [
        ("run_terminal_command", ToolCategory::Shell),
        ("read_file", ToolCategory::FileRead),
        ("list_dir", ToolCategory::FileRead),
        ("write", ToolCategory::FileWrite),
        ("search_replace", ToolCategory::FileWrite),
        ("grep", ToolCategory::Search),
        ("web_search", ToolCategory::Search),
        ("web_fetch", ToolCategory::Search),
        ("open_page", ToolCategory::Search),
        ("open_page_with_find", ToolCategory::Search),
        ("x_posts_search", ToolCategory::Search),
        ("spawn_subagent", ToolCategory::Agent),
        ("workflow", ToolCategory::Workflow),
        ("search_tool", ToolCategory::Mcp),
        ("use_tool", ToolCategory::Mcp),
    ];
    for (name, expected) in by_name {
        assert_eq!(
            categorize(Some(name), Some("other")),
            expected,
            "name {name} must beat kind"
        );
    }
    // Two kind-only fallbacks: an unknown name is classified by its kind.
    assert_eq!(
        categorize(Some("future_execute_tool"), Some("execute")),
        ToolCategory::Shell
    );
    assert_eq!(
        categorize(Some("future_edit_tool"), Some("edit")),
        ToolCategory::FileWrite
    );
    assert_eq!(
        categorize(Some("future_write_tool"), Some("write")),
        ToolCategory::FileWrite
    );
    assert_eq!(
        categorize(Some("ask_user_question"), Some("ask_user")),
        ToolCategory::Other
    );
    // Last resort: no table name, no usable kind.
    assert_eq!(categorize(None, None), ToolCategory::Other);
    assert_eq!(
        categorize(Some("mystery"), Some("mystery-kind")),
        ToolCategory::Other
    );
}

#[test]
fn statusless_update_merges_fields_and_only_updates_known_calls() {
    // Synthesized from docs, not captured [U].
    let updates = [
        frame(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-merge",
            "title": "Initial title",
            "rawInput": { "command": "echo one" },
            "_meta": { "x.ai/tool": { "name": "run_terminal_command", "kind": "execute" } }
        })),
        // Progress frame omits title/rawInput: old values survive.
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-merge",
            "kind": "execute",
            "_meta": { "x.ai/tool": { "name": "run_terminal_command", "kind": "execute" } }
        })),
        // A second progress frame refreshes just the title.
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-merge",
            "title": "Execute `echo two`",
            "_meta": { "x.ai/tool": { "name": "run_terminal_command", "kind": "execute" } }
        })),
        // A progress frame for an unknown id is dropped…
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-stranger",
            "title": "Execute `stranger`"
        })),
        // …but its terminal frame still gets a result.
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-stranger",
            "status": "completed",
            "rawOutput": { "output_for_prompt": "stranger ok\n", "exit_code": 0 }
        })),
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-merge",
            "status": "completed",
            "rawOutput": { "output_for_prompt": "exit: 0\n", "exit_code": 0 }
        })),
    ]
    .concat();
    let (_dir, mut adapter) = adapter_for_updates(&updates);
    let observed = adapter.poll().unwrap();

    let calls = tool_calls(&observed);
    assert_eq!(calls.len(), 3, "proposal + two progress frames");
    assert_eq!(calls[1].state, remuda_protocol::ToolCallState::Running);
    assert_eq!(calls[1].mutation.revision, remuda_protocol::U64(2));
    assert_eq!(
        calls[1].display_title,
        remuda_protocol::Knowledge::Known {
            value: "Initial title".into()
        }
    );
    let kept_input = match &calls[1].input {
        remuda_protocol::Knowledge::Known { value } => value,
        other => panic!("input kept: {other:?}"),
    };
    assert_eq!(kept_input["command"], json!("echo one"));
    assert_eq!(calls[2].mutation.revision, remuda_protocol::U64(3));
    assert_eq!(
        calls[2].display_title,
        remuda_protocol::Knowledge::Known {
            value: "Execute `echo two`".into()
        }
    );

    let results = tool_results(&observed);
    assert_eq!(results.len(), 2);
    // The known call closes strictly above its last call revision (3 → 4);
    // the stranger gets today's 1 → 2 shape.
    let merge = results
        .iter()
        .find(|r| r.tool_call_id == calls[2].tool_call_id)
        .expect("merge result");
    assert_eq!(merge.mutation.revision, remuda_protocol::U64(4));
    assert_eq!(merge.mutation.base_revision, Some(remuda_protocol::U64(3)));
    let stranger: Vec<_> = observed
        .iter()
        .filter(|o| o.item_id.as_deref() == Some("call-stranger"))
        .collect();
    assert_eq!(stranger.len(), 1, "dropped progress produced no call fact");
    let stranger_result = match &stranger[0].payload {
        ObservationPayload::ToolResult(result) => result,
        other => panic!("expected stranger result, got {other:?}"),
    };
    assert_eq!(stranger_result.mutation.revision, remuda_protocol::U64(2));
}

#[test]
fn diff_terminal_and_unknown_content_blocks_are_surfaced_synthesized() {
    // Synthesized from docs, not captured [U]: the 1.0.30 fixture carries no
    // diff/terminal content blocks; revisit at the 1.0.34 recapture.
    let completed = json!({
        "status": "completed",
        "locations": [{ "path": "/workspace/other.txt" }],
        "content": [
            { "type": "content", "content": { "type": "text", "text": "wrote" } },
            { "type": "diff", "path": "/workspace/a.txt", "diff": "@@ -1 +1 @@\n-old\n+new\n" }
        ]
    });
    let (blocks, changes, has_content_text) = tool_content(&completed, true);
    assert_eq!(blocks.len(), 1);
    assert_eq!(changes.len(), 1);
    assert!(has_content_text);
    assert_eq!(changes[0].path, "/workspace/a.txt");
    assert!(changes[0].diff.contains("+new"));
    assert_eq!(
        changes[0].application,
        remuda_protocol::ChangeApplication::Applied
    );

    // Path falls back to the first location; a failed/error frame keeps the
    // change Unknown rather than claiming it applied.
    let failed = json!({
        "status": "failed",
        "error": "apply rejected",
        "locations": [{ "path": "/workspace/located.txt" }],
        "content": [{ "type": "diff", "diff": { "path": "/workspace/located.txt", "patch": "@@ " } }]
    });
    let (blocks, changes, has_content_text) = tool_content(&failed, false);
    assert!(blocks.is_empty());
    assert!(!has_content_text);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "/workspace/located.txt");
    assert_eq!(changes[0].diff, "@@ ");
    assert_eq!(
        changes[0].application,
        remuda_protocol::ChangeApplication::Unknown
    );

    // Terminal references are text markers, never tty-attach promises; typed
    // non-text content and unknown outer types are markers too. None of those
    // markers count as content text, so output_for_prompt still applies.
    let mixed = json!({
        "content": [
            { "type": "terminal", "terminalId": "term-7", "path": "terminal/x.log" },
            { "type": "terminal" },
            { "type": "content", "content": { "type": "image", "data": "…" } },
            { "type": "future_block" }
        ]
    });
    let (blocks, changes, has_content_text) = tool_content(&mixed, false);
    assert!(changes.is_empty());
    assert!(!has_content_text);
    let texts: Vec<String> = blocks
        .iter()
        .filter_map(|block| match block {
            remuda_protocol::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "[grok terminal: term-7]".to_owned(),
            "[grok terminal]".to_owned(),
            "[grok content block: image]".to_owned(),
            "[grok content block: future_block]".to_owned(),
        ]
    );
}

#[test]
fn terminal_only_content_keeps_the_real_output_for_prompt_synthesized() {
    // Synthesized from docs, not captured [U]: a shell call whose content[] is
    // just a terminal reference must still surface rawOutput.output_for_prompt
    // — markers must not suppress the real command output (regression guard).
    let updates = [
        frame(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-term",
            "title": "run_terminal_command",
            "rawInput": { "command": "echo hi" },
            "_meta": { "x.ai/tool": { "name": "run_terminal_command", "kind": "execute" } }
        })),
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-term",
            "status": "completed",
            "content": [
                { "type": "terminal", "terminalId": "term-1", "path": "terminal/call-term.log" }
            ],
            "rawOutput": {
                "type": "Bash",
                "output_for_prompt": "hi\nexit: 0\n",
                "exit_code": 0
            }
        })),
    ]
    .concat();
    let (_dir, mut adapter) = adapter_for_updates(&updates);
    let observed = adapter.poll().unwrap();
    let results = tool_results(&observed);
    assert_eq!(results.len(), 1);
    let texts: Vec<&str> = results[0]
        .blocks
        .iter()
        .filter_map(|block| match block {
            remuda_protocol::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("term-1")),
        "terminal marker kept: {texts:?}"
    );
    assert!(
        texts.contains(&"hi\nexit: 0\n"),
        "real output_for_prompt present: {texts:?}"
    );
}

#[test]
fn in_progress_runs_and_unknown_statuses_stay_open_synthesized() {
    // Synthesized from docs, not captured [U]: protocol §5.7 — pending /
    // in_progress are not terminal, unknown statuses stay opaque.
    let updates = [
        frame(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-status",
            "title": "run_terminal_command",
            "rawInput": { "command": "sleep 1" },
            "_meta": { "x.ai/tool": { "name": "run_terminal_command", "kind": "execute" } }
        })),
        // in_progress behaves like the statusless progress frame.
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-status",
            "status": "in_progress",
            "title": "Execute `sleep 1`"
        })),
        // An unknown future status must not close the node with a result.
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-status",
            "status": "deferred_by_future_build"
        })),
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-status",
            "status": "completed",
            "rawOutput": { "output_for_prompt": "exit: 0\n", "exit_code": 0 }
        })),
    ]
    .concat();
    let (_dir, mut adapter) = adapter_for_updates(&updates);
    let observed = adapter.poll().unwrap();

    let calls = tool_calls(&observed);
    assert_eq!(calls.len(), 2, "proposal + one Running, not per status");
    assert_eq!(calls[0].state, remuda_protocol::ToolCallState::Proposed);
    assert_eq!(calls[0].mutation.revision, remuda_protocol::U64(1));
    assert_eq!(calls[1].state, remuda_protocol::ToolCallState::Running);
    assert_eq!(calls[1].mutation.revision, remuda_protocol::U64(2));

    let results = tool_results(&observed);
    assert_eq!(results.len(), 1, "unknown status produced no result");
    assert_eq!(results[0].mutation.revision, remuda_protocol::U64(3));
    assert_eq!(results[0].outcome, ToolOutcome::Succeeded);

    // A standalone pending update also leaves the node untouched.
    let pending_only = [
        frame(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-pending",
            "title": "run_terminal_command",
            "rawInput": {},
            "_meta": { "x.ai/tool": { "name": "run_terminal_command", "kind": "execute" } }
        })),
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-pending",
            "status": "pending"
        })),
    ]
    .concat();
    let (_dir2, mut adapter2) = adapter_for_updates(&pending_only);
    let observed2 = adapter2.poll().unwrap();
    assert_eq!(tool_calls(&observed2).len(), 1);
    assert!(tool_results(&observed2).is_empty());
}

#[test]
fn finished_tracks_release_their_tool_inputs_synthesized() {
    // Synthesized from docs, not captured [U].
    let updates = [
        frame(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-clear",
            "title": "run_terminal_command",
            "rawInput": { "command": "echo large-payload" },
            "_meta": { "x.ai/tool": { "name": "run_terminal_command", "kind": "execute" } }
        })),
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-clear",
            "status": "completed",
            "rawOutput": { "output_for_prompt": "exit: 0\n", "exit_code": 0 }
        })),
    ]
    .concat();
    let (_dir, mut adapter) = adapter_for_updates(&updates);
    adapter.poll().unwrap();
    let track = adapter
        .tools
        .get("call-clear")
        .expect("track retained for dedupe");
    assert!(track.finished);
    assert!(track.input.is_none());
    assert!(track.name.is_none());
    assert!(track.display_title.is_none());
    assert!(track.kind.is_none());
}

#[test]
fn a_completed_diff_frame_emits_non_empty_changes_end_to_end() {
    // Synthesized from docs, not captured [U].
    let updates = [
        frame(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-diff",
            "title": "write_file",
            "rawInput": { "file_path": "/workspace/a.txt" },
            "_meta": { "x.ai/tool": { "name": "write", "kind": "write" } }
        })),
        frame(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-diff",
            "status": "completed",
            "content": [
                { "type": "diff", "path": "/workspace/a.txt", "diff": "@@ -0,0 +1 @@\n+hello\n" }
            ],
            "rawOutput": { "exit_code": 0 }
        })),
    ]
    .concat();
    let (_dir, mut adapter) = adapter_for_updates(&updates);
    let observed = adapter.poll().unwrap();
    let calls = tool_calls(&observed);
    assert_eq!(calls[0].category, remuda_protocol::ToolCategory::FileWrite);
    let results = tool_results(&observed);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].changes.len(), 1);
    assert_eq!(results[0].changes[0].path, "/workspace/a.txt");
    assert_eq!(
        results[0].changes[0].application,
        remuda_protocol::ChangeApplication::Applied
    );
}

#[test]
fn thought_closes_with_the_full_text_and_mirror_status() {
    // Synthesized from docs, not captured [U].
    let updates = [
        frame(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "reasoning-" },
            "_meta": { "promptId": "pt" }
        })),
        frame(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "done" },
            "_meta": { "promptId": "pt" }
        })),
        frame(json!({
            "sessionUpdate": "turn_completed",
            "prompt_id": "pt",
            "stop_reason": "cancelled"
        })),
    ]
    .concat();
    let (_dir, mut adapter) = adapter_for_updates(&updates);
    let observed = adapter.poll().unwrap();
    let thoughts: Vec<_> = observed
        .iter()
        .filter_map(|o| match &o.payload {
            ObservationPayload::Thought(thought) => Some(thought.as_ref()),
            _ => None,
        })
        .collect();
    // open chunk, append chunk, close snapshot.
    assert_eq!(thoughts.len(), 3);
    assert_eq!(
        thoughts[0].mutation.operation,
        remuda_protocol::MutationOperation::Open
    );
    assert_eq!(
        thoughts[1].mutation.operation,
        remuda_protocol::MutationOperation::Append
    );
    let close = thoughts[2];
    assert_eq!(
        close.mutation.operation,
        remuda_protocol::MutationOperation::Close
    );
    assert_eq!(close.mutation.revision, remuda_protocol::U64(3));
    assert_eq!(close.mutation.base_revision, Some(remuda_protocol::U64(2)));
    assert_eq!(close.text.as_deref(), Some("reasoning-done"));
    assert_eq!(close.status, ContentStatus::Interrupted);
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
