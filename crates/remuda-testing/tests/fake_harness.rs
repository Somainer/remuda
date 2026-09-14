//! End-to-end tests for `fake-harness` through a real PTY.
//!
//! Covers: per-dialect artifacts parsed by the production parsers, hook
//! round-trips (including blocking PermissionRequest), the verified input
//! semantics (claude boundary queue / Esc survival / body+CR, codex
//! steer/Tab/Esc, grok queue / empty-Enter send-now / Esc / double Ctrl+C),
//! and `--resume`.

mod support;

use std::time::Duration;

use remuda_driver::claude_transcript::TranscriptTail;
use remuda_driver::codex_rollout::{CodexRolloutEvent, parse_rollout_line};
use remuda_driver::grok_session::{
    SessionSelector, parse_active_sessions, parse_event_line, parse_update_line,
};
use serde_json::Value;
use support::{HarnessBuilder, Key, hooks_dir};

const WAIT: Duration = Duration::from_secs(8);

fn jsonl(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("read jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid json"))
        .collect()
}

// ===========================================================================
// Artifacts parsed by the real parsers
// ===========================================================================

#[test]
fn claude_artifacts_parse_with_the_transcript_mapper() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("claude")
        .scenario("approval.json")
        .spawn();
    h.submit("RUN_TOOL");
    // Native approval dialog: single approval.
    h.wait_event("approval_prompt", |_| true, WAIT);
    h.press_digit(1);
    h.wait_exit(WAIT);
    let path = h
        .find_file("00000000-0000-4000-8000-000000000001.jsonl")
        .expect("transcript");
    let records = jsonl(&path);

    // One record per content block: text and tool_use arrive separately with
    // apiBlockIndex indices.
    let assistant_blocks: Vec<(i64, String)> = records
        .iter()
        .filter(|r| r.get("type").and_then(Value::as_str) == Some("assistant"))
        .map(|r| {
            let block = &r["message"]["content"][0];
            (
                r.get("apiBlockIndex").and_then(Value::as_i64).unwrap_or(-1),
                block.get("type").unwrap().as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        assistant_blocks.iter().any(|(_, kind)| kind == "tool_use"),
        "missing tool_use block: {assistant_blocks:?}"
    );
    assert!(
        assistant_blocks.iter().any(|(_, kind)| kind == "text"),
        "missing text block: {assistant_blocks:?}"
    );
    let indices: Vec<i64> = assistant_blocks.iter().map(|(idx, _)| *idx).collect();
    assert!(indices.windows(2).all(|w| w[0] <= w[1]));

    let tool_result = records
        .iter()
        .find(|r| {
            r.pointer("/message/content/0/type").and_then(Value::as_str) == Some("tool_result")
        })
        .expect("tool_result record");
    assert_eq!(
        tool_result["message"]["content"][0]["is_error"].as_bool(),
        Some(false)
    );

    // The production tail sees the same whole lines.
    let mut tail = TranscriptTail::new(path.clone());
    let lines = tail.poll().expect("poll");
    assert_eq!(lines.len(), records.len());
}

#[test]
fn codex_artifacts_parse_with_the_rollout_parser() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("codex")
        .scenario("approval.json")
        .spawn();
    h.submit("RUN_TOOL");
    h.press_digit(1);
    h.wait_exit(WAIT);
    let path = find_rollout(h.home()).expect("rollout file");

    let lines = std::fs::read_to_string(&path).expect("read rollout");
    let events: Vec<_> = lines
        .lines()
        .map(|line| parse_rollout_line(line).expect("parse rollout line"))
        .collect();

    assert!(
        events
            .iter()
            .any(|r| matches!(r.event, CodexRolloutEvent::SessionMeta { .. }))
    );
    assert!(
        events
            .iter()
            .any(|r| matches!(r.event, CodexRolloutEvent::TaskStarted { .. }))
    );
    let tool_call = events
        .iter()
        .find_map(|r| match &r.event {
            CodexRolloutEvent::ToolCall { name, call_id, .. } => Some((name, call_id)),
            _ => None,
        })
        .expect("function call");
    // The model calls exec_command; the normalized Bash name is only on hooks.
    assert_eq!(tool_call.0.as_deref(), Some("exec_command"));
    assert!(
        events
            .iter()
            .any(|r| matches!(r.event, CodexRolloutEvent::ToolOutput { .. }))
    );
    assert!(
        events
            .iter()
            .any(|r| matches!(r.event, CodexRolloutEvent::TaskComplete { .. }))
    );
    assert!(
        events
            .iter()
            .any(|r| matches!(r.event, CodexRolloutEvent::TokenUsage { .. }))
    );
    assert!(events.iter().all(|r| r.ordinal.is_some()));
    // session_index.jsonl names the thread.
    let index = h.read("session_index.jsonl");
    let row: Value = serde_json::from_str(index.lines().next().unwrap()).unwrap();
    assert_eq!(row["id"], "00000000-0000-4000-8000-000000000001");
}

#[test]
fn grok_artifacts_parse_with_the_session_parser_and_locate_works() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("grok")
        .scenario("approval.json")
        .spawn();
    h.submit("RUN_TOOL");
    h.wait_event("approval_prompt", |_| true, WAIT);
    // Registry is populated while the TUI is alive.
    let registry = h.read("active_sessions.json");
    let sessions = parse_active_sessions(&registry).expect("active_sessions.json");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].pid, h.pid());
    let located =
        remuda_driver::grok_session::locate_session(h.home(), SessionSelector::Pid(h.pid()))
            .expect("locate by pid")
            .expect("unique match");
    assert!(located.directory.join("updates.jsonl").is_file());

    h.press_digit(2); // "Yes, proceed"
    h.wait_exit(WAIT);

    let updates_path = h.find_file("updates.jsonl").expect("updates.jsonl");
    let kinds: Vec<String> = std::fs::read_to_string(&updates_path)
        .expect("read")
        .lines()
        .map(|line| {
            let record = parse_update_line(line).expect("parse update");
            format!("{:?}", record.update)
        })
        .collect();
    assert!(kinds.iter().any(|k| k.contains("UserMessageChunk")));
    assert!(kinds.iter().any(|k| k.contains("ToolCall")));
    assert!(kinds.iter().any(|k| k.contains("ToolCallUpdate")));
    assert!(kinds.iter().any(|k| k.contains("AgentMessageChunk")));
    // turn_completed is the grok extension method.
    let last_line = std::fs::read_to_string(&updates_path)
        .unwrap()
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap()
        .to_owned();
    let last = serde_json::from_str::<Value>(&last_line).unwrap();
    assert_eq!(last["method"], "_x.ai/session/update");
    assert_eq!(last["params"]["update"]["sessionUpdate"], "turn_completed");
    assert_eq!(last["params"]["update"]["stop_reason"], "end_turn");

    let events_path = support::find_walk(&h.home().join("sessions"), "events.jsonl")
        .expect("session events.jsonl");
    let event_kinds: Vec<String> = std::fs::read_to_string(&events_path)
        .unwrap()
        .lines()
        .map(|line| {
            parse_event_line(line)
                .map(|r| format!("{:?}", r.event))
                .unwrap_or_default()
        })
        .collect();
    assert!(event_kinds.iter().any(|k| k.contains("TurnStarted")));
    assert!(event_kinds.iter().any(|k| k.contains("FirstToken")));
    // Registry entry is removed at shutdown, before process exit.
    let registry = h.read("active_sessions.json");
    assert_eq!(registry.trim(), "[]");
}

// ===========================================================================
// Hooks
// ===========================================================================

fn hook_env(log: &std::path::Path, decision: Option<&str>, style: &str) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "FAKE_HARNESS_FIXTURE_DIR".to_owned(),
            hooks_dir().to_string_lossy().into_owned(),
        ),
        (
            "FAKE_HARNESS_HOOK_LOG".to_owned(),
            log.to_string_lossy().into_owned(),
        ),
        ("FAKE_HARNESS_HOOK_STYLE".to_owned(), style.to_owned()),
    ];
    if let Some(decision) = decision {
        env.push(("FAKE_HARNESS_HOOK_DECISION".to_owned(), decision.to_owned()));
    }
    env
}

#[test]
fn claude_hooks_round_trip_with_observed_events() {
    let _serial = support::serial();
    let log = std::env::temp_dir().join(format!("fake-harness-hook-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let mut builder = HarnessBuilder::new("claude")
        .scenario("hooks.json")
        .settings(hooks_dir().join("claude-settings.json"));
    for (k, v) in &hook_env(&log, Some("allow"), "claude") {
        builder = builder.env(k, v);
    }
    let mut h = builder.spawn();
    h.submit("HOOK");
    h.wait_exit(WAIT);

    let lines = std::fs::read_to_string(&log).expect("hook log");
    let events: Vec<Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).expect("hook log json"))
        .collect();
    let names: Vec<&str> = events
        .iter()
        .map(|row| row.get("event").and_then(Value::as_str).unwrap_or(""))
        .collect();
    for required in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
        "Stop",
        "SessionEnd",
    ] {
        assert!(
            names.contains(&required),
            "missing hook event {required}: {names:?}"
        );
    }
    let session_start = events
        .iter()
        .find(|row| row["event"] == "SessionStart")
        .expect("SessionStart stdin");
    let transcript_path = session_start["payload"]["transcript_path"]
        .as_str()
        .expect("SessionStart must let the production driver bind its transcript");
    assert_eq!(
        std::fs::canonicalize(transcript_path).unwrap(),
        std::fs::canonicalize(
            h.find_file("00000000-0000-4000-8000-000000000001.jsonl")
                .expect("native transcript")
        )
        .unwrap()
    );
    let permission = events
        .iter()
        .find(|row| row["event"] == "PermissionRequest")
        .expect("permission stdin");
    assert_eq!(permission["payload"]["tool_name"], "Bash");
    assert!(
        permission["payload"].get("tool_use_id").is_some(),
        "claude PermissionRequest carries tool_use_id: {permission}"
    );
    assert!(permission["env"]["CLAUDE_PROJECT_DIR"].as_str().is_some());
    // MessageDisplay fires on streamed lines.
    assert!(names.contains(&"MessageDisplay"));
    let _ = std::fs::remove_file(&log);
}

#[test]
fn codex_permission_request_hook_blocks_then_allows() {
    let _serial = support::serial();
    let log = std::env::temp_dir().join(format!(
        "fake-harness-codex-hook-{}.log",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&log);
    let wait_file =
        std::env::temp_dir().join(format!("fake-harness-codex-decide-{}", std::process::id()));
    let _ = std::fs::remove_file(&wait_file);
    let home = support::temp_home();
    std::fs::copy(
        hooks_dir().join("codex-hooks.json"),
        home.join("hooks.json"),
    )
    .expect("copy hooks.json");
    let mut builder = HarnessBuilder::new("codex")
        .home(home)
        .scenario("hooks.json");
    let mut env = hook_env(&log, Some("allow"), "codex");
    env.push((
        "FAKE_HARNESS_HOOK_WAIT_FILE".to_owned(),
        wait_file.to_string_lossy().into_owned(),
    ));
    for (k, v) in &env {
        builder = builder.env(k, v);
    }
    let mut h = builder.spawn();
    h.submit("HOOK");

    // The hook is blocking: wait until it has been invoked, then allow.
    wait_for_log(&log, "PermissionRequest", WAIT);
    assert!(!h.saw_event("turn_end"));
    std::fs::write(&wait_file, b"allow").expect("write decision");
    h.wait_exit(WAIT);

    // stdin shape: no tool_use_id, tool normalized to Bash, codex wrapper.
    let line = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(serde_json::from_str::<Value>)
        .map(Result::unwrap)
        .find(|row| row["event"] == "PermissionRequest")
        .expect("permission request");
    assert!(line["payload"].get("tool_use_id").is_none());
    assert_eq!(line["payload"]["tool_name"], "Bash");
    assert_eq!(
        line["payload"]["tool_input"]["command"],
        "printf hook-approved"
    );

    // The tool actually ran: no rejection in the rollout.
    let rollout = find_rollout(h.home()).expect("rollout");
    let body = std::fs::read_to_string(&rollout).unwrap();
    assert!(!body.contains("Rejected"));
    assert!(body.contains("exit_code"));
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_file(&wait_file);
}

#[test]
fn grok_pre_tool_use_deny_blocks_even_in_auto_and_permissionrequest_is_ignored() {
    let _serial = support::serial();
    let log =
        std::env::temp_dir().join(format!("fake-harness-grok-hook-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let home = support::temp_home();
    std::fs::create_dir_all(home.join("hooks")).unwrap();
    std::fs::copy(
        hooks_dir().join("grok-hooks/probe.json"),
        home.join("hooks/probe.json"),
    )
    .unwrap();
    let mut builder = HarnessBuilder::new("grok").home(home).scenario("slow.json");
    for (k, v) in &hook_env(&log, Some("deny"), "grok") {
        builder = builder.env(k, v);
    }
    let mut h = builder.spawn();
    h.submit("SLOW");
    h.wait_event("turn_end", |_| true, WAIT);

    let names: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .map(|row| row["event"].as_str().unwrap_or_default().to_owned())
                .unwrap_or_default()
        })
        .collect();
    assert!(names.contains(&"PreToolUse".to_owned()));
    // PermissionRequest is silently dropped by grok's loader.
    assert!(!names.contains(&"PermissionRequest".to_owned()));

    let updates = support::find_walk(&h.home().join("sessions"), "updates.jsonl")
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_default();
    assert!(updates.contains("Hook denied"));
    h.shutdown();
    let _ = std::fs::remove_file(&log);
}

fn wait_for_log(path: &std::path::Path, event: &str, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.lines().any(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|row| row["event"] == event)
            })
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for {event} in {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn find_rollout(home: &std::path::Path) -> Option<std::path::PathBuf> {
    support::find_walk(home, "rollout-")
}

// ===========================================================================
// Input semantics
// ===========================================================================

#[test]
fn claude_body_and_cr_in_one_write_does_not_submit() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("claude").scenario("ok.json").spawn();
    // One write containing body + CR, as the evidence requires not to submit.
    h.write_raw(b"hello\r");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !h.saw_event("submit"),
        "body+CR in one write submitted the prompt"
    );
    // A separate Enter submits the drafted text.
    h.press_enter();
    h.wait_exit(WAIT);
}

#[test]
fn claude_enter_enqueues_and_boundary_consumes_with_remove_and_attachment() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("claude").scenario("slow.json").spawn();
    h.submit("SLOW_QUEUE");
    // Enqueue while the first sleep runs.
    h.type_text("STEER_FOLLOWUP");
    h.press_enter();
    let delivered = h.wait_event(
        "boundary_deliver",
        |v| v.get("reason").and_then(Value::as_str) == Some("absorbed_mid_turn"),
        WAIT,
    );
    assert!(delivered["count"].as_u64().unwrap_or(0) >= 1);
    h.shutdown();

    let transcript = h
        .find_file("00000000-0000-4000-8000-000000000001.jsonl")
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_default();
    assert!(transcript.contains("\"operation\":\"enqueue\""));
    assert!(transcript.contains("\"operation\":\"remove\""));
    assert!(transcript.contains("\"reason\":\"absorbed_mid_turn\""));
    assert!(transcript.contains("\"type\":\"attachment\""));
    assert!(transcript.contains("\"queued_command\""));
}

#[test]
fn claude_esc_interrupts_and_the_queue_survives_as_a_new_user_record() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("claude").scenario("slow.json").spawn();
    h.submit("SLOW_INTERRUPT");
    h.type_text("QUEUED_AFTER_ESC");
    h.press_enter();
    h.wait_event("enqueue", |_| true, WAIT);
    h.press(Key::Esc);
    // Queued text becomes a top-level user record and runs on.
    h.wait_event(
        "turn_start",
        |v| v.get("redirect").and_then(Value::as_str) == Some("dequeue_after_interrupt"),
        WAIT,
    );
    h.shutdown();
    let transcript = h
        .find_file("00000000-0000-4000-8000-000000000001.jsonl")
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_default();
    assert!(transcript.contains("\"operation\":\"dequeue\""));
    assert!(transcript.contains("User rejected tool use"));
    assert!(transcript.contains("QUEUED_AFTER_ESC"));
}

#[test]
fn codex_enter_steers_into_the_same_turn() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("codex").scenario("slow.json").spawn();
    h.submit("SLOW_STEER_BASE");
    let base_turn = h.wait_event("turn_start", |_| true, WAIT);
    assert!(base_turn.get("turn_id").is_none()); // turn id lives in the rollout
    h.type_text("STEER_FOLLOWUP");
    h.press_enter();
    h.wait_event("steer", |v| v["content"] == "STEER_FOLLOWUP", WAIT);
    // Let the turn finish.
    h.wait_event("turn_end", |_| true, WAIT);
    h.shutdown();

    let rollout = find_rollout(h.home()).expect("rollout");
    let lines = std::fs::read_to_string(&rollout).unwrap();
    let rows: Vec<Value> = lines
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let task_started: Vec<&str> = rows
        .iter()
        .filter(|r| r["payload"]["type"] == "task_started")
        .map(|r| r["payload"]["turn_id"].as_str().unwrap())
        .collect();
    // Base and steered user messages share the single turn id.
    assert_eq!(task_started.len(), 1, "steer must not open a new turn");
    let turn_id = task_started[0];
    let user_turns: Vec<&str> = rows
        .iter()
        .filter(|r| r["payload"]["type"] == "message" && r["payload"]["role"] == "user")
        .map(|r| {
            r["payload"]["internal_chat_message_metadata_passthrough"]["turn_id"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert!(user_turns.iter().all(|t| *t == turn_id));
    assert_eq!(user_turns.len(), 2);
}

#[test]
fn codex_tab_queues_for_the_next_turn() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("codex").scenario("slow.json").spawn();
    h.submit("SLOW_QUEUE_BASE");
    h.wait_event("turn_start", |_| true, WAIT);
    h.type_text("QUEUED_FOLLOWUP");
    h.press(Key::Tab);
    h.wait_event(
        "queue",
        |v| v.get("key").and_then(Value::as_str) == Some("tab"),
        WAIT,
    );
    // The queued item opens a second turn after the first completes.
    let second = h.wait_event("turn_start", |_| true, WAIT);
    assert!(second.get("redirect").and_then(Value::as_str) == Some("queued_next_turn"));
    h.wait_exit(WAIT);
}

#[test]
fn codex_esc_aborts_the_turn_with_turn_aborted() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("codex").scenario("slow.json").spawn();
    h.submit("SLOW_ESC_BASE");
    h.wait_event("turn_start", |_| true, WAIT);
    h.press(Key::Esc);
    h.wait_any(&["interrupt"], WAIT);
    h.shutdown();
    let rollout = find_rollout(h.home()).expect("rollout");
    let body = std::fs::read_to_string(&rollout).unwrap();
    assert!(body.contains("\"type\":\"turn_aborted\""));
    assert!(body.contains("\"reason\":\"interrupted\""));
}

#[test]
fn grok_enter_queues_for_next_turn() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("grok").scenario("slow.json").spawn();
    h.submit("SLOW_QUEUE");
    h.wait_event("turn_start", |_| true, WAIT);
    h.type_text("QUEUED_FOLLOWUP");
    h.press_enter();
    h.wait_event("queue", |_| true, WAIT);
    // Normal queue delivery carries no fabricated redirect_kind (evidence A1).
    h.wait_event("turn_start", |_| true, WAIT);
    h.wait_exit(WAIT);
    let events = support::find_walk(&h.home().join("sessions"), "events.jsonl")
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_default();
    assert!(events.contains("\"type\":\"turn_ended\""));
}

#[test]
fn grok_empty_enter_cancels_and_sends_queued_item() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("grok").scenario("slow.json").spawn();
    h.submit("SLOW_SENDNOW");
    h.wait_event("turn_start", |_| true, WAIT);
    h.type_text("NOW_FOLLOWUP");
    h.press_enter();
    h.wait_event("queue", |_| true, WAIT);
    // Empty Enter on the empty composer cancels + sends now.
    h.press_enter();
    let interrupt = h.wait_event("interrupt", |v| v["by"] == "send_now", WAIT);
    assert_eq!(interrupt["by"], "send_now");
    let second = h.wait_event("turn_start", |_| true, WAIT);
    assert_eq!(second["redirect"], "queued_after_cancel");
    let updates_path =
        support::find_walk(&h.home().join("sessions"), "updates.jsonl").expect("updates");
    h.wait_exit(WAIT);
    let updates = std::fs::read_to_string(&updates_path).unwrap();
    assert!(updates.contains("\"stop_reason\":\"cancelled\""));
}

#[test]
fn grok_esc_keeps_the_turn_and_draft() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("grok").scenario("slow.json").spawn();
    h.submit("SLOW_ESC");
    h.wait_event("turn_start", |_| true, WAIT);
    h.type_text("UNSENT_DRAFT");
    h.press(Key::Esc);
    h.wait_event("esc_notice", |v| v["draft_preserved"] == true, WAIT);
    // No interrupt for a short window: the turn keeps running to completion.
    std::thread::sleep(Duration::from_millis(200));
    assert!(!h.saw_event("interrupt"));
    h.wait_event("turn_end", |_| true, WAIT);
    h.shutdown();
}

#[test]
fn grok_double_ctrl_c_cancels_single_ctrl_c_does_not() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("grok").scenario("slow.json").spawn();
    h.submit("SLOW_CTRLC");
    h.wait_event("turn_start", |_| true, WAIT);
    h.press(Key::CtrlC);
    std::thread::sleep(Duration::from_millis(150));
    assert!(!h.saw_event("interrupt"));
    h.press(Key::CtrlC);
    h.wait_event("interrupt", |v| v["by"] == "ctrl_c", WAIT);
    let events_path =
        support::find_walk(&h.home().join("sessions"), "events.jsonl").expect("events");
    h.shutdown();
    let events = std::fs::read_to_string(&events_path).unwrap();
    assert!(events.contains("\"trigger\":\"ctrl_c\""));
    assert!(events.contains("\"outcome\":\"cancelled\""));
}

#[test]
fn grok_ctrl_c_with_draft_only_clears_the_draft() {
    let _serial = support::serial();
    let mut h = HarnessBuilder::new("grok").scenario("slow.json").spawn();
    h.submit("SLOW_DRAFT");
    h.wait_event("turn_start", |_| true, WAIT);
    h.type_text("UNSENT_DRAFT");
    h.press(Key::CtrlC);
    h.wait_event("ctrl_c_clear_draft", |_| true, WAIT);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!h.saw_event("interrupt"));
    h.shutdown();
}

// ===========================================================================
// Resume
// ===========================================================================

#[test]
fn codex_resume_appends_to_the_same_rollout_with_one_session_meta() {
    let _serial = support::serial();
    let home = support::temp_home();
    let session = "00000000-0000-4000-8000-000000000001";
    {
        let mut h = HarnessBuilder::new("codex")
            .home(home.clone())
            .scenario("ok.json")
            .arg("--session-id")
            .arg(session)
            .spawn();
        h.submit("first prompt");
        h.wait_exit(WAIT);
    }
    {
        let mut h = HarnessBuilder::new("codex")
            .home(home.clone())
            .scenario("ok.json")
            .arg("--resume")
            .arg(session)
            .spawn();
        h.submit("second prompt");
        h.wait_exit(WAIT);
    }
    let rollout = find_rollout(&home).expect("single rollout");
    let body = std::fs::read_to_string(&rollout).unwrap();
    assert_eq!(body.matches("\"type\":\"session_meta\"").count(), 1);
    assert_eq!(body.matches("\"type\":\"task_started\"").count(), 2);
    assert!(body.contains("first prompt"));
    assert!(body.contains("second prompt"));
}

// ===========================================================================
// Flags
// ===========================================================================

#[test]
fn binary_rejects_unknown_kind() {
    let _serial = support::serial();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fake-harness"))
        .args(["--kind", "nope"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown kind"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn yaml_scenario_loads() {
    let _serial = support::serial();
    // The YAML fixture must parse and select its exact-match turn.
    let mut h = HarnessBuilder::new("claude").scenario("demo.yaml").spawn();
    h.submit("YAML_RUN");
    h.wait_exit(WAIT);
    let transcript = h
        .find_file("00000000-0000-4000-8000-000000000001.jsonl")
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_default();
    assert!(transcript.contains("SPIKE_COMPLETE YAML"));
}
