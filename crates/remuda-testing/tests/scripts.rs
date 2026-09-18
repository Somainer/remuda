//! Drive bundled `fake-claude` scripts over stream-json NDJSON.

use remuda_testing::{
    FIXED_SESSION_ID, FakeClaudeProcess, ScriptKind, SpawnOptions, fake_claude_argv, fixtures_dir,
    is_control_subtype, is_system_subtype, is_type, spawn_fake_claude, spawn_fake_claude_sdk,
    transcript_path,
};
use serde_json::{Value, json};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

fn boot(kind: ScriptKind) -> FakeClaudeProcess {
    let mut child = spawn_fake_claude(SpawnOptions::bundled(kind)).expect("spawn fake-claude");
    let init = child
        .recv_until(TIMEOUT, |v| is_system_subtype(v, "init"))
        .expect("system/init");
    assert_eq!(
        init.get("session_id").and_then(Value::as_str),
        Some(FIXED_SESSION_ID)
    );
    let tools = init.get("tools").and_then(Value::as_array).expect("tools");
    for required in ["Workflow", "Task", "SendMessage"] {
        assert!(
            tools.iter().any(|t| t.as_str() == Some(required)),
            "missing {required}"
        );
    }
    child.send_initialize("init-test").expect("send initialize");
    let reply = child
        .recv_until(TIMEOUT, |v| {
            is_type(v, "control_response")
                && v.pointer("/response/request_id").and_then(Value::as_str) == Some("init-test")
        })
        .expect("initialize ack");
    assert_eq!(
        reply.pointer("/response/subtype").and_then(Value::as_str),
        Some("success")
    );
    child
}

#[test]
fn ok_script_returns_ok_result() {
    let mut child = boot(ScriptKind::Ok);
    child.send_user("hi").unwrap();
    let result = child.recv_until(TIMEOUT, |v| is_type(v, "result")).unwrap();
    assert_eq!(result.get("result").and_then(Value::as_str), Some("OK"));
    assert_eq!(
        result.get("subtype").and_then(Value::as_str),
        Some("success")
    );
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn approval_script_allow_and_deny() {
    let mut child = boot(ScriptKind::Approval);
    child.send_user("touch a file").unwrap();
    let ask = child
        .recv_until(TIMEOUT, |v| is_control_subtype(v, "can_use_tool"))
        .unwrap();
    assert_eq!(
        ask.pointer("/request/tool_name").and_then(Value::as_str),
        Some("Bash")
    );
    let request_id = ask.get("request_id").and_then(Value::as_str).unwrap();
    child
        .send_control_response(
            request_id,
            json!({
                "behavior": "allow",
                "updatedInput": {
                    "command": "touch /tmp/remuda-fake-allow.txt",
                    "description": "Create probe file"
                }
            }),
        )
        .unwrap();
    let result = child.recv_until(TIMEOUT, |v| is_type(v, "result")).unwrap();
    assert_eq!(result.get("result").and_then(Value::as_str), Some("Done."));
    let status = child.wait().unwrap();
    assert!(status.success());

    let mut child = boot(ScriptKind::Approval);
    child.send_user("touch a file").unwrap();
    let ask = child
        .recv_until(TIMEOUT, |v| is_control_subtype(v, "can_use_tool"))
        .unwrap();
    let request_id = ask.get("request_id").and_then(Value::as_str).unwrap();
    child
        .send_control_response(
            request_id,
            json!({
                "behavior": "deny",
                "message": "host denied"
            }),
        )
        .unwrap();
    let result = child.recv_until(TIMEOUT, |v| is_type(v, "result")).unwrap();
    assert!(
        result
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("denied")
    );
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn askuser_script_round_trip() {
    let mut child = boot(ScriptKind::AskUser);
    child.send_user("ask me").unwrap();
    let ask = child
        .recv_until(TIMEOUT, |v| is_control_subtype(v, "can_use_tool"))
        .unwrap();
    assert_eq!(
        ask.pointer("/request/tool_name").and_then(Value::as_str),
        Some("AskUserQuestion")
    );
    let request_id = ask.get("request_id").and_then(Value::as_str).unwrap();
    child
        .send_control_response(
            request_id,
            json!({
                "behavior": "allow",
                "updatedInput": {
                    "questions": ask.pointer("/request/input/questions").cloned().unwrap_or(json!([])),
                    "answers": { "Tea or coffee?": "Tea" }
                }
            }),
        )
        .unwrap();
    let result = child.recv_until(TIMEOUT, |v| is_type(v, "result")).unwrap();
    assert!(
        result
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("tea")
    );
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn workflow_script_emits_task_frames_and_two_results() {
    let mut child = boot(ScriptKind::Workflow);
    child.send_user("run workflow").unwrap();
    let started = child
        .recv_until(TIMEOUT, |v| is_system_subtype(v, "task_started"))
        .unwrap();
    assert_eq!(
        started.get("task_type").and_then(Value::as_str),
        Some("local_workflow")
    );
    child
        .recv_until(TIMEOUT, |v| is_system_subtype(v, "task_progress"))
        .unwrap();
    let first = child.recv_until(TIMEOUT, |v| is_type(v, "result")).unwrap();
    assert!(
        first
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("Waiting")
    );
    child
        .recv_until(TIMEOUT, |v| is_system_subtype(v, "task_notification"))
        .unwrap();
    let second = child.recv_until(TIMEOUT, |v| is_type(v, "result")).unwrap();
    assert_eq!(second.get("result").and_then(Value::as_str), Some("OK"));
    assert_eq!(second.get("result_index").and_then(Value::as_i64), Some(1));
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn interrupt_ends_turn_while_waiting_for_approval() {
    let mut child = boot(ScriptKind::Approval);
    child.send_user("touch").unwrap();
    child
        .recv_until(TIMEOUT, |v| is_control_subtype(v, "can_use_tool"))
        .unwrap();
    child.send_interrupt("int-1").unwrap();
    let result = child
        .recv_until(TIMEOUT, |v| {
            is_type(v, "result")
                && v.get("stop_reason").and_then(Value::as_str) == Some("interrupt")
        })
        .unwrap();
    assert_eq!(
        result.get("result").and_then(Value::as_str),
        Some("Interrupted")
    );
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn transcript_dir_appends_user_assistant_result() {
    let dir = std::env::temp_dir().join(format!("remuda-fake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut opts = SpawnOptions::bundled(ScriptKind::Ok);
    opts.transcript_dir = Some(dir.clone());
    let mut child = spawn_fake_claude(opts).unwrap();
    child
        .recv_until(TIMEOUT, |v| is_system_subtype(v, "init"))
        .unwrap();
    child.send_initialize("init-tr").unwrap();
    child
        .recv_until(TIMEOUT, |v| is_type(v, "control_response"))
        .unwrap();
    child.send_user("hi").unwrap();
    child.recv_until(TIMEOUT, |v| is_type(v, "result")).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
    let path = transcript_path(&dir, FIXED_SESSION_ID);
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("\"type\":\"user\""));
    assert!(body.contains("\"type\":\"assistant\""));
    assert!(body.contains("\"type\":\"result\""));
    assert!(body.contains(FIXED_SESSION_ID));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn captured_fixtures_are_present() {
    let root = fixtures_dir();
    for rel in [
        "SOURCES.md",
        "claude/claude-p-init.json",
        "claude/claude-permission-host-allow.jsonl",
        "claude/claude-askuser.jsonl",
        "claude/claude-workflow-canary-1.jsonl",
        "codex/codex-appserver-session.jsonl",
        "grok/grok-headless-streaming-json.jsonl",
        "agy/agy-stream-json-sample.jsonl",
        "scripts/ok.jsonl",
        "scripts/approval.jsonl",
        "scripts/askuser.jsonl",
        "scripts/workflow.jsonl",
        "scripts/twoturn.jsonl",
    ] {
        let path = root.join(rel);
        assert!(path.is_file(), "missing {}", path.display());
    }
}

#[test]
fn fake_claude_prints_version_and_exits() {
    let bin = env!("CARGO_BIN_EXE_fake-claude");
    let output = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .expect("run fake-claude --version");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("2.1.268"),
        "unexpected --version output: {stdout}"
    );
}

/// The one flag that separates the carriers. `-p` is "Print response and exit";
/// the sdk template omits it and is otherwise byte-identical, so the print
/// spawn keeps working unchanged (`print-replacement.md` §2.1, §3 batch 1).
#[test]
fn sdk_spawn_argv_has_no_dash_p_and_print_still_does() {
    let print = fake_claude_argv(FIXED_SESSION_ID, true);
    let sdk = fake_claude_argv(FIXED_SESSION_ID, false);
    assert_eq!(print.first().map(String::as_str), Some("-p"));
    assert!(!sdk.iter().any(|a| a == "-p" || a == "--print"));
    assert_eq!(sdk, print[1..].to_vec());
    // Golden argv: stream-json both ways plus the host permission channel.
    assert_eq!(
        sdk,
        vec![
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "default",
            "--permission-prompts",
            "host",
            "--permission-prompt-tool",
            "stdio",
            "--session-id",
            FIXED_SESSION_ID,
        ]
    );
}

/// The carrier claim: one child, two turns, two results, same session id.
///
/// A `-p` child would be gone after the first `result`, which is the whole
/// reason print cannot be a worker carrier (§2.1, D-035). `ClaudeFlags` still
/// parses `-p`, so this passes on the sdk argv without changing the fake.
#[test]
fn sdk_argv_survives_two_turns_on_one_child() {
    let mut child = spawn_fake_claude_sdk(SpawnOptions::bundled(ScriptKind::TwoTurn))
        .expect("spawn fake-claude");
    child
        .recv_until(TIMEOUT, |v| is_system_subtype(v, "init"))
        .expect("system/init");

    child.send_user("first").unwrap();
    let first = child.recv_until(TIMEOUT, |v| is_type(v, "result")).unwrap();
    assert_eq!(first.get("result").and_then(Value::as_str), Some("First."));
    assert_eq!(first.get("result_index").and_then(Value::as_u64), Some(0));

    // Same child, no relaunch and no resume: in-session turns are further
    // `user` frames on the open stdin (§2.2).
    child.send_user("second").unwrap();
    let second = child.recv_until(TIMEOUT, |v| is_type(v, "result")).unwrap();
    assert_eq!(
        second.get("result").and_then(Value::as_str),
        Some("Second.")
    );
    assert_eq!(second.get("result_index").and_then(Value::as_u64), Some(1));
    assert_eq!(
        second.get("session_id").and_then(Value::as_str),
        first.get("session_id").and_then(Value::as_str)
    );

    let status = child.wait().unwrap();
    assert!(status.success());
}
