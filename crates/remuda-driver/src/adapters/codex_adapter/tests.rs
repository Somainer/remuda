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
        launched_at: None,
        resume_session_id: None,
    });
    adapter.confirm_session(session_id);
    assert!(adapter.discover().unwrap());
    assert!(adapter.binding().is_some());
    adapter
}

fn feed(adapter: &mut CodexAdapter, lines: &str) -> Vec<AdapterObservation> {
    let mut out = Vec::new();
    // Compute each record's first-byte offset within the fixture string, as a
    // real RolloutTail would (Ruling B2).
    let mut ordinal = 0u64;
    let mut cursor = 0usize;
    for raw in lines.split_inclusive('\n') {
        let start = cursor;
        cursor += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        ordinal += 1;
        out.extend(adapter.on_line(ordinal, "feed-fixture", start as u64, line));
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
        launched_at: None,
        resume_session_id: None,
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
        launched_at: None,
        resume_session_id: None,
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
        launched_at: None,
        resume_session_id: None,
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
        launched_at: None,
        resume_session_id: None,
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
{"ordinal":5,"timestamp":"2026-09-13T18:21:45.380Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}
"#;
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: tempfile::tempdir().unwrap().path().to_path_buf(),
        cwd: std::path::PathBuf::from("/w"),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    });
    let observed = feed(&mut adapter, lines);
    let usages: Vec<_> = observed
        .iter()
        .filter(|o| matches!(o.payload, ObservationPayload::Usage(_)))
        .collect();
    // c-usagefu r6 item 5: codex emits ONE billable stream — the per-response
    // Message snapshot — followed by the cumulative Session snapshot. The
    // Turn snapshot was removed because the Hub summed Message and Turn as
    // two billable streams and double-counted priced/unpriced/priced turns.
    assert_eq!(usages.len(), 2);
    let message = match &usages[0].payload {
        ObservationPayload::Usage(usage) => usage,
        _ => unreachable!(),
    };
    let session = match &usages[1].payload {
        ObservationPayload::Usage(usage) => usage,
        _ => unreachable!(),
    };
    assert_eq!(message.scope, remuda_protocol::UsageScope::Message);
    assert_eq!(session.scope, remuda_protocol::UsageScope::Session);
    // r12 item 3 (Ruling B2'): even a record WITH a native response id is
    // keyed on the durable (rollout file identity, byte offset) pair.
    let offset = lines.find(r#"{"ordinal":3"#).unwrap() as u64;
    assert_eq!(
        message.scope_id,
        super::durable_response_scope_id("feed-fixture", offset)
    );
    // c-ctxusage r5 item 7: the Session row carries the rollout record's own
    // timestamp, so a byte-0 replay is historical evidence, not
    // ingest-time throughput. The task_complete record stamps ...:45.380Z.
    assert_eq!(
        String::from(
            usages[1]
                .native_at
                .as_ref()
                .expect("usage carries native_at")
                .clone()
        ),
        "2026-09-13T18:21:45.380Z",
        "native_at is the task_complete record time"
    );
    assert_eq!(session.accounting, remuda_protocol::Accounting::Estimated);
    // Total spans every bucket: 90 uncached input + 10 cache read + 20 output
    // (the same definition usage::tests locks in).
    assert_eq!(
        session.total_tokens,
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
        launched_at: None,
        resume_session_id: None,
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
        launched_at: None,
        resume_session_id: None,
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

#[test]
fn native_timestamp_parses_rollout_record_time() {
    let value = remuda_driver::adapters::native_timestamp(Some("2026-09-13T18:21:45.380Z"));
    assert!(value.is_some(), "rfc3339 ms parses");
    let none = remuda_driver::adapters::native_timestamp(None);
    assert!(none.is_none());
}

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Write one active rollout with a session_meta header carrying the thread id,
/// working directory and session-start time, under a date-sharded dir.
fn write_rollout(home: &Path, date: &str, id: &str, cwd: &str, started: &str) {
    // Discovery ownership requires a real first user prompt (r5 item 5); the
    // unit-test driver "sent" the fixture prompt for these launches.
    write_rollout_with_prompt(home, date, id, cwd, started, "fixture driver prompt");
}

/// The c-usagefu r3 item 1 regression: on generic-pty the adapter tails the
/// operator's REAL codex home (shared with every project), has no hooks and no
/// child pid. The last entry in `session_index.jsonl` is simply the thread the
/// operator renamed last — possibly in another project — and must never be
/// bound. Only a unique same-cwd session started at/after our launch binds.
#[test]
fn a_driver_launch_binds_only_its_own_post_launch_same_cwd_rollout() {
    let home = tempfile::tempdir().unwrap();
    let cwd = Path::new("/projects/alpha-r3bind");
    // Relative times: floor ten seconds ago keeps the 120 s discovery window
    // open for the test's duration.
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(10);
    let rfc = |at: OffsetDateTime| at.format(&Rfc3339).unwrap();

    // The operator's pre-existing sessions: one in ANOTHER cwd that started
    // after our floor (cwd filter), one in our cwd that started BEFORE the
    // launch (time filter). The name index's last line points at the foreign
    // thread — under the old code that was exactly what got bound.
    write_rollout(
        home.path(),
        "2026/09/14",
        "foreign-other-cwd",
        "/projects/beta-r3bind",
        &rfc(now - time::Duration::seconds(1)),
    );
    write_rollout(
        home.path(),
        "2026/09/13",
        "foreign-same-cwd-old",
        "/projects/alpha-r3bind",
        &rfc(now - time::Duration::seconds(600)),
    );
    std::fs::write(
        home.path().join("session_index.jsonl"),
        "{\"id\":\"foreign-same-cwd-old\"}\n{\"id\":\"foreign-other-cwd\"}\n",
    )
    .unwrap();

    let mut adapter = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_instance_key(Some(INST_A.to_owned()));

    assert!(adapter.poll().unwrap().is_empty());
    let bound = adapter.binding().map(|binding| binding.session_id.clone());
    assert!(
        bound.is_none(),
        "no foreign session may bind, got {bound:?}"
    );

    // The pane's own session registers shortly after launch; the driver
    // dispatched its prompt just before that.
    send_fixture_prompt(cwd, INST_A, "fixture driver prompt", now);
    write_rollout(
        home.path(),
        "2026/09/14",
        "own-session",
        "/projects/alpha-r3bind",
        &rfc(now - time::Duration::seconds(2)),
    );
    assert!(adapter.discover().unwrap());
    assert_eq!(
        adapter.binding().map(|binding| binding.session_id.as_str()),
        Some("own-session"),
        "only the unique post-launch same-cwd rollout binds"
    );
}

/// r8 item 1: two same-cwd prompt-compatible sessions are ambiguous and stay
/// UNBOUND while both exist (no guess), but discovery never latches a
/// permanent give-up — once the competing candidate goes away, the sole
/// remaining candidate is attributable in the sole (never-shared) epoch.
#[test]
fn two_post_launch_same_cwd_rollouts_stay_unbound_then_resolve_when_unique() {
    let home = tempfile::tempdir().unwrap();
    let cwd = Path::new("/projects/alpha-r8amb");
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(10);
    let rfc = |at: OffsetDateTime| at.format(&Rfc3339).unwrap();
    send_fixture_prompt(cwd, INST_A, "fixture driver prompt", now);
    write_rollout(
        home.path(),
        "2026/09/14",
        "one",
        "/projects/alpha-r8amb",
        &rfc(now - time::Duration::seconds(5)),
    );
    write_rollout(
        home.path(),
        "2026/09/14",
        "two",
        "/projects/alpha-r8amb",
        &rfc(now - time::Duration::seconds(1)),
    );

    let mut adapter = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_instance_key(Some(INST_A.to_owned()));
    assert!(!adapter.discover().unwrap(), "two candidates bind nothing");
    assert!(adapter.binding().is_none());
    assert!(!adapter.discovery_gave_up, "ambiguity never latches");

    // Remove the competing candidate: the remaining sole file is the only
    // prompt-compatible rollout in a never-shared epoch → it binds.
    std::fs::remove_file(
        home.path()
            .join("sessions/2026/09/14")
            .join("rollout-two.jsonl"),
    )
    .unwrap();
    assert!(
        adapter.discover().unwrap(),
        "unique remaining candidate binds"
    );
    assert_eq!(
        adapter.binding().map(|b| b.session_id.as_str()),
        Some("one")
    );
}

/// codex 0.154 creates the rollout LAZILY on the first turn: an instance
/// first prompted long after pane start (well past the old 120 s window) must
/// still bind when its rollout appears. Discovery keeps running until bound;
/// there is no time deadline (c-usagefu r4 item 2).
#[test]
fn a_rollout_appearing_long_after_launch_binds_when_the_first_prompt_is_late() {
    let home = tempfile::tempdir().unwrap();
    let cwd = Path::new("/projects/alpha-r4late");
    // Floor 200 s ago — under the old deadline this adapter had given up.
    let floor = OffsetDateTime::now_utc() - time::Duration::seconds(200);
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_instance_key(Some(INST_A.to_owned()));
    assert!(!adapter.discover().unwrap());
    assert!(adapter.binding().is_none());
    assert!(
        !adapter.discovery_gave_up,
        "NotYet never gives up; the poll loop keeps discovering until bound"
    );

    // First prompt at t+200 s: the driver dispatches it and the rollout's
    // session starts just now.
    let now = OffsetDateTime::now_utc();
    send_fixture_prompt(cwd, INST_A, "fixture driver prompt", now);
    let started = now.format(&Rfc3339).unwrap();
    write_rollout(
        home.path(),
        "2026/09/14",
        "late-prompt",
        "/projects/alpha-r4late",
        &started,
    );
    assert!(
        adapter.discover().unwrap(),
        "the lazily-created post-launch rollout binds"
    );
    assert_eq!(
        adapter.binding().map(|binding| binding.session_id.as_str()),
        Some("late-prompt")
    );
}

/// A rollout that physically exists long after launch but whose session
/// STARTED before it is a pre-launch session: the content timestamp, not the
/// file mtime, decides ownership, so it never binds.
#[test]
fn a_fresh_file_carrying_a_pre_launch_session_never_binds() {
    let home = tempfile::tempdir().unwrap();
    let floor = OffsetDateTime::now_utc() - time::Duration::seconds(5);
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: Path::new("/projects/alpha-r4old").to_path_buf(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    });
    let old = (floor - time::Duration::seconds(600))
        .format(&Rfc3339)
        .unwrap();
    write_rollout(
        home.path(),
        "2026/09/14",
        "old-session",
        "/projects/alpha-r4old",
        &old,
    );
    assert!(!adapter.discover().unwrap());
    assert!(adapter.binding().is_none());
}

/// Write a rollout that mirrors real 0.154 ordering: injected environment
/// context, then the REAL first user prompt as an `input_text` item.
fn write_rollout_with_prompt(
    home: &Path,
    date: &str,
    id: &str,
    cwd: &str,
    started: &str,
    prompt: &str,
) {
    let session_dir = home.join("sessions").join(date);
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join(format!("rollout-{id}.jsonl"));
    let env_block = "<environment_context> <cwd>/x</cwd> </environment_context>";
    std::fs::write(
        path,
        format!(
            "{{\"timestamp\":\"{started}\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"session_id\":\"{id}\",\"cwd\":\"{cwd}\",\"timestamp\":\"{started}\"}}}}\n             {{\"timestamp\":\"{started}\",\"ordinal\":1,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m_env\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{env_block}\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"environments.environment_context\"]}}}}}}\n             {{\"timestamp\":\"{started}\",\"ordinal\":2,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m_user\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{prompt}\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"user.text\"]}}}}}}\n"
        ),
    )
    .unwrap();
}

/// Per-instance keys used by the driver-launch unit tests.
const INST_A: &str = "inst-a-key";
const INST_B: &str = "inst-b-key";
const INST_C: &str = "inst-c-key";

/// Record a driver-sent prompt (as generic_pty.send does) for unit tests.
/// `cwd` must be the same path given to the adapter's `AdapterHome`.
fn send_fixture_prompt(cwd: &Path, owner: &str, prompt: &str, at: OffsetDateTime) {
    crate::adapters::codex_discovery::record_input_at(cwd, owner, prompt, at);
}

/// Build an unbound driver-launch adapter (with an open discovery window).
fn launch_adapter(home: &Path, cwd: &str, floor: OffsetDateTime) -> CodexAdapter {
    launch_adapter_keyed(home, cwd, floor, INST_A)
}

/// Build a driver-launch adapter with an explicit per-instance identity.
fn launch_adapter_keyed(home: &Path, cwd: &str, floor: OffsetDateTime, key: &str) -> CodexAdapter {
    CodexAdapter::new(AdapterHome {
        home: home.to_path_buf(),
        cwd: Path::new(cwd).to_path_buf(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_instance_key(Some(key.to_owned()))
}

/// r8 item 1: driver-launch adapter with an injected process-ownership
/// probe + shared pid-root cell. Tests set/clear the fake roots and tell the
/// FakeOwnership which files the agent process currently holds open.
fn launch_with_fake_ownership(
    home: &Path,
    cwd: &str,
    floor: OffsetDateTime,
    key: &str,
    pid: u32,
    probe: &std::sync::Arc<crate::adapters::codex_ownership::FakeOwnership>,
    roots: &std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
) -> CodexAdapter {
    launch_adapter_keyed(home, cwd, floor, key)
        .with_ownership(
            probe.clone() as std::sync::Arc<dyn crate::adapters::codex_ownership::RolloutOwnership>,
            Some(roots.clone()),
        )
        .with_fake_pid(pid)
}

/// Test-only builder setting a fake single pid root (avoids pub fields).
trait WithFakePid {
    fn with_fake_pid(self, pid: u32) -> Self;
}
impl WithFakePid for CodexAdapter {
    fn with_fake_pid(self, _pid: u32) -> Self {
        self // roots are supplied through the Arc; kept for readability at callsites
    }
}

/// Make a fake ownership probe and a shared pid-root cell seeded with `pid`.
fn fake_ownership(
    pid: u32,
) -> (
    std::sync::Arc<crate::adapters::codex_ownership::FakeOwnership>,
    std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
) {
    (
        std::sync::Arc::new(crate::adapters::codex_ownership::FakeOwnership::new()),
        std::sync::Arc::new(std::sync::Mutex::new(vec![pid])),
    )
}

#[test]
fn a_bound_first_instance_releases_its_window_and_a_later_instance_binds_its_own() {
    // r5 item 1: A binds at 10:01 and its window must not linger; B launched
    // in the same cwd later is never tainted, and A's claimed rollout is
    // excluded for B, so B binds B's own file.
    //
    // r9 item 1: A stays ALIVE for the whole test, so prompt text alone can
    // no longer attribute B's rollout (A is another live instance in the
    // cwd) — B binds its own file through the process-ownership tier, exactly
    // as the generic-pty wiring does on Linux.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r5-sequential";
    let now = OffsetDateTime::now_utc();
    // Relative timing: A launched long ago; B launches a little later and
    // both session timestamps are real (past) so file mtime pruning does not
    // shadow the assertion. B's floor is strictly after A's session start.
    let floor_a = now - time::Duration::seconds(60);
    let a_started = (now - time::Duration::seconds(50))
        .format(&Rfc3339)
        .unwrap();

    send_fixture_prompt(
        Path::new(cwd),
        INST_A,
        "fixture driver prompt",
        now - time::Duration::seconds(50),
    );
    let mut a = launch_adapter_keyed(home.path(), cwd, floor_a, INST_A);
    write_rollout(home.path(), "2026/09/14", "A-thread", cwd, &a_started);
    assert!(a.discover().unwrap(), "A binds its unique rollout");
    assert!(
        a.window.is_none(),
        "binding releases the discovery window immediately"
    );

    // B launches later with a fresh floor after A's session started. A is
    // still live, so prompt-only attribution is off: B's process probe
    // decides.
    let floor_b = now - time::Duration::seconds(10);
    let (probe_b, roots_b) = fake_ownership(8002);
    let mut b =
        launch_with_fake_ownership(home.path(), cwd, floor_b, INST_B, 8002, &probe_b, &roots_b);
    assert!(
        !b.window.as_ref().is_some_and(|window| window.overlapping()),
        "A is no longer discovering: B opens clean"
    );
    assert!(
        !b.discover().unwrap(),
        "A's claimed rollout must not bind to B"
    );
    assert!(b.binding().is_none());

    // B's own lazily-created rollout appears after B's prompt; unique,
    // unclaimed, and held open by B's agent process -> binds.
    send_fixture_prompt(
        Path::new(cwd),
        INST_B,
        "fixture driver prompt",
        now - time::Duration::seconds(2),
    );
    let later = (now - time::Duration::seconds(2)).format(&Rfc3339).unwrap();
    write_rollout(home.path(), "2026/09/14", "B-thread", cwd, &later);
    probe_b.hold(
        &home
            .path()
            .join("sessions/2026/09/14")
            .join("rollout-B-thread.jsonl"),
        8002,
    );
    assert!(b.discover().unwrap());
    assert_eq!(
        b.binding().map(|binding| binding.session_id.as_str()),
        Some("B-thread")
    );
    // And A still owns A.
    assert_eq!(
        a.binding().map(|binding| binding.session_id.as_str()),
        Some("A-thread")
    );
}

#[test]
fn three_overlapping_windows_do_not_latch_give_up_and_clear_as_they_close() {
    // Three generic-pty codex panes in one cwd launched together: while they
    // overlap, an ambiguity (two files appearing) must WAIT, never give up
    // permanently; the claimed set keeps an already-bound file out of the
    // match set; closing seekers un-taints the survivors.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r5-three";
    let floor = OffsetDateTime::now_utc() - time::Duration::seconds(5);
    let now = OffsetDateTime::now_utc();

    let mut a = launch_adapter_keyed(home.path(), cwd, floor, INST_A);
    let mut b = launch_adapter_keyed(home.path(), cwd, floor, INST_B);
    let mut c = launch_adapter_keyed(home.path(), cwd, floor, INST_C);
    for adapter in [&a, &b, &c] {
        assert!(
            adapter
                .window
                .as_ref()
                .is_some_and(|window| window.overlapping()),
            "three seekers overlap"
        );
    }

    // r8 item 1: with three live discoverers, prompt text is disabled (the
    // epoch is shared). Only A's process-held rollout binds.
    let (probe_a, roots_a) = fake_ownership(3001);
    let (probe_b, roots_b) = fake_ownership(3002);
    let (probe_c, roots_c) = fake_ownership(3003);
    a = a.with_ownership(
        probe_a.clone() as std::sync::Arc<dyn crate::adapters::codex_ownership::RolloutOwnership>,
        Some(roots_a.clone()),
    );
    b = b.with_ownership(
        probe_b.clone() as std::sync::Arc<dyn crate::adapters::codex_ownership::RolloutOwnership>,
        Some(roots_b.clone()),
    );
    c = c.with_ownership(
        probe_c.clone() as std::sync::Arc<dyn crate::adapters::codex_ownership::RolloutOwnership>,
        Some(roots_c.clone()),
    );

    write_rollout_prompt_at(
        home.path(),
        "A1",
        cwd,
        &now.format(&Rfc3339).unwrap(),
        "A distinct words",
    );
    let a1_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-A1.jsonl");
    probe_a.hold(&a1_path, 3001);

    // Exactly one poll across the three: A's process owns A1; B/C probes do
    // not hold it → unbound and STILL discovering (no give-up latch).
    let mut bound = None;
    for adapter in [&mut a, &mut b, &mut c] {
        if adapter.discover().unwrap() {
            bound = Some(
                adapter
                    .binding()
                    .map(|binding| binding.session_id.clone())
                    .unwrap(),
            );
        }
    }
    assert_eq!(bound.as_deref(), Some("A1"), "only A's process binds A1");
    for adapter in [&b, &c] {
        assert!(
            !adapter.discovery_gave_up,
            "a stronger tier may still arrive: no permanent give-up"
        );
        assert!(adapter.binding().is_none());
    }

    // Idle B closes without ever binding: C un-taints but A is already bound
    // (window released), so C alone may wait for its own file.
    drop(b);
    assert!(
        !c.window.as_ref().is_some_and(|window| window.overlapping()),
        "closing an idle seeker clears the overlap"
    );
}

#[test]
fn an_idle_unbound_instance_with_a_live_window_does_not_block_b() {
    // r8 item 1: A launched but never prompted stays ALIVE the whole time. B
    // launches in the same cwd and binds its own rollout via the PROCESS-tier
    // probe (prompt-only attribution is off in a shared epoch).
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r8-idle";
    let floor = OffsetDateTime::now_utc() - time::Duration::seconds(5);
    let (probe_a, roots_a) = fake_ownership(4001);
    let (probe_b, roots_b) = fake_ownership(4002);
    let mut a =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_A, 4001, &probe_a, &roots_a);
    assert!(!a.discover().unwrap(), "A has no rollout yet");

    let mut b =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_B, 4002, &probe_b, &roots_b);
    assert!(
        b.window.as_ref().is_some_and(|window| window.overlapping()),
        "B overlaps the still-idle, still-LIVE A"
    );

    // B's rollout appears, held open by B's agent pid (4002).
    let now = OffsetDateTime::now_utc();
    write_rollout(
        home.path(),
        "2026/09/14",
        "B-only",
        cwd,
        &now.format(&Rfc3339).unwrap(),
    );
    let b_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-B-only.jsonl");
    probe_b.hold(&b_path, 4002);
    assert!(b.discover().unwrap(), "B binds with A's window still live");
    assert_eq!(
        b.binding().map(|binding| binding.session_id.as_str()),
        Some("B-only")
    );
    // A's probe holds nothing → remains idle and unbound throughout.
    assert!(!a.discover().unwrap());
    assert!(a.binding().is_none());
}

#[test]
fn a_strictly_post_launch_match_hydrates_its_own_first_turn_from_byte_zero() {
    // r5 item 2: codex 0.154 creates the rollout lazily ON the first turn, so
    // session_meta and task_started share one timestamp; reading from EOF
    // dropped the instance's own first turn (task_started, turn_context, the
    // prompt and its usage). A session whose session_meta.timestamp is at/after
    // the STRICT launch floor must be hydrated from byte zero.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r5-strict-tail";
    let now = OffsetDateTime::now_utc();
    // Floor well in the past: session start is strictly after it (and the
    // content is genuinely new — no previous session at this timestamp).
    let floor = now - time::Duration::seconds(30);
    let started = now.format(&Rfc3339).unwrap();
    let session_dir = home.path().join("sessions/2026/09/14");
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join("rollout-strict.jsonl");
    // Real 0.154 first turn: env context + the dispatched user prompt ...
    write_rollout_with_prompt(
        home.path(),
        "2026/09/14",
        "strict",
        cwd,
        &started,
        "fixture driver prompt",
    );
    // ... plus the turn's first task_started, which byte-zero hydration must
    // surface.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{{\"timestamp\":\"{started}\",\"ordinal\":3,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"first-turn\"}}}}"
        )
        .unwrap();
    }

    let mut adapter = launch_adapter(home.path(), cwd, floor);
    send_fixture_prompt(Path::new(cwd), INST_A, "fixture driver prompt", now);
    assert!(
        adapter.discover().unwrap(),
        "the strict post-floor file matches"
    );
    let first_poll = adapter.poll().unwrap();
    let turns: Vec<_> = first_poll
        .iter()
        .filter_map(|obs| obs.turn_id.clone())
        .collect();
    assert_eq!(
        turns,
        vec!["first-turn"],
        "the instance's OWN first turn is hydrated from byte zero, got {first_poll:?}"
    );

    // Subsequent appends still tail normally (the head is not replayed).
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        file,
        "{{\"timestamp\":\"{started}\",\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"second-turn\"}}}}"
    )
    .unwrap();
    drop(file);
    let next = adapter.poll().unwrap();
    let turns: Vec<_> = next.iter().filter_map(|obs| obs.turn_id.clone()).collect();
    assert_eq!(turns, vec!["second-turn"]);
}

#[test]
fn an_in_slack_match_tails_from_end_and_skips_pre_bind_records() {
    // A session started INSIDE the clock slack (session_meta.timestamp before
    // the strict floor) could be a crash-looped previous instance. r9 item 1:
    // prompt text can NEVER attribute an in-slack candidate — the 5 s
    // pre-launch slack is reserved for the hook and process-ownership tiers.
    // With process proof the adapter binds the in-slack file and tails from
    // EOF, so its pre-bind records never replay.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r5-slack-tail";
    let now = OffsetDateTime::now_utc();
    let floor = now; // strict floor = now; session 2 s earlier is in-slack only
    let started = (now - time::Duration::seconds(2)).format(&Rfc3339).unwrap();
    let session_dir = home.path().join("sessions/2026/09/14");
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join("rollout-slack.jsonl");
    write_rollout_with_prompt(
        home.path(),
        "2026/09/14",
        "slack",
        cwd,
        &started,
        "fixture driver prompt",
    );
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{{\"timestamp\":\"{started}\",\"ordinal\":3,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"old-turn\"}}}}"
        )
        .unwrap();
    }

    let mut adapter = launch_adapter(home.path(), cwd, floor);
    send_fixture_prompt(Path::new(cwd), INST_A, "fixture driver prompt", now);
    assert!(
        !adapter.discover().unwrap(),
        "prompt text never attributes a pre-launch (in-slack) file"
    );
    assert!(
        adapter.binding().is_none(),
        "the in-slack file stays unbound pending stronger evidence"
    );

    // The process tier proves ownership of the in-slack file: it now binds,
    // strictly=false, so the tail starts at EOF.
    let (probe, roots) = fake_ownership(9001);
    probe.hold(&path, 9001);
    let mut adapter = adapter.with_ownership(
        probe.clone() as std::sync::Arc<dyn crate::adapters::codex_ownership::RolloutOwnership>,
        Some(roots),
    );
    assert!(
        adapter.discover().unwrap(),
        "process ownership accepts the in-slack candidate"
    );
    let pre_bind = adapter.poll().unwrap();
    assert!(
        pre_bind.is_empty(),
        "in-slack records must not replay, got {pre_bind:?}"
    );

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let now_rfc = now.format(&Rfc3339).unwrap();
    writeln!(
        file,
        "{{\"timestamp\":\"{now_rfc}\",\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"new-turn\"}}}}"
    )
    .unwrap();
    drop(file);
    let post_bind = adapter.poll().unwrap();
    let turns: Vec<_> = post_bind
        .iter()
        .filter_map(|obs| obs.turn_id.clone())
        .collect();
    assert_eq!(turns, vec!["new-turn"]);
}

#[test]
fn a_hand_run_codex_in_the_same_cwd_is_never_bound_by_an_idle_instance() {
    // r5 item 5: an unprompted Remuda codex pane launched in /proj sits idle;
    // hours later the operator runs codex BY HAND in /proj. The foreign
    // session is post-floor and same-cwd (and unique), but its first user
    // message is not an input this driver sent -> discovery never binds it.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r5-handrun";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let mut adapter = launch_adapter(home.path(), cwd, floor);

    // The operator's hand-run session, created "now" with the operator's own
    // prompt. The driver never sent anything in this cwd.
    let started = now.format(&Rfc3339).unwrap();
    write_rollout_with_prompt(
        home.path(),
        "2026/09/14",
        "hand-run",
        cwd,
        &started,
        "do the thing by hand",
    );
    assert!(
        !adapter.discover().unwrap(),
        "a foreign hand-run prompt must not bind"
    );
    assert!(adapter.binding().is_none());

    // Still foreign on later polls (no unbounded-discovery leak).
    assert!(!adapter.discover().unwrap());
    assert!(adapter.binding().is_none());
}

#[test]
fn a_rollout_whose_first_prompt_matches_a_driver_sent_input_binds() {
    // Positive control: the SAME setup binds once the driver records sending
    // that prompt (as generic_pty.send does), provided the timing lines up.
    use crate::adapters::codex_discovery::record_input_at;

    let home = tempfile::tempdir().unwrap();
    let cwd_dir = home.path().join("proj-r5-sent");
    std::fs::create_dir_all(&cwd_dir).unwrap();
    let cwd = cwd_dir.to_str().unwrap();
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let prompt = "remuda dispatched this";

    let mut adapter = launch_adapter_keyed(home.path(), cwd, floor, "sent-key");
    // The driver sent the prompt slightly before the rollout was created.
    record_input_at(&cwd_dir, "sent-key", prompt, now);
    let started = (now - time::Duration::seconds(1)).format(&Rfc3339).unwrap();
    write_rollout_with_prompt(home.path(), "2026/09/14", "sent-one", cwd, &started, prompt);
    assert!(
        adapter.discover().unwrap(),
        "a matching driver-sent prompt binds the own session"
    );
    assert_eq!(
        adapter.binding().map(|binding| binding.session_id.as_str()),
        Some("sent-one")
    );
}

// c-usagefu r6 item 1 ---------------------------------------------------------

fn write_rollout_prompt_at(home: &Path, id: &str, cwd: &str, started: &str, prompt: &str) {
    write_rollout_with_prompt(home, "2026/09/14", id, cwd, started, prompt);
}

/// Reverse tick order: B polls BEFORE A has polled. Even though B sees A's
/// prompt file first, the prompt text is A's — B's gate rejects it, and B
/// then binds its own distinct rollout.
#[test]
fn a_prompt_file_is_never_bound_by_the_instance_that_polls_first() {
    // r8 item 1: in a shared epoch prompt text cannot attribute a rollout;
    // the PROCESS-TIER probe decides. B scans first but holds no fd on A's
    // file; A's fake probe holds A-first, so A binds; B binds B-second.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r8-pollorder";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let rfc = now.format(&Rfc3339).unwrap();

    let (probe_a, roots_a) = fake_ownership(1001);
    let (probe_b, roots_b) = fake_ownership(2002);

    write_rollout_prompt_at(home.path(), "A-first", cwd, &rfc, "A prompt words");
    let a_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-A-first.jsonl");
    probe_a.hold(&a_path, 1001);

    // B scans first: no fd in B's tree → stays unbound.
    let mut b =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_B, 2002, &probe_b, &roots_b);
    assert!(
        !b.discover().unwrap(),
        "B must not bind A's process-owned file"
    );
    assert!(b.binding().is_none());

    // A binds even though B polled first.
    let mut a =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_A, 1001, &probe_a, &roots_a);
    assert!(
        a.discover().unwrap(),
        "A binds the file its process holds open"
    );
    assert_eq!(a.binding().map(|b| b.session_id.as_str()), Some("A-first"));

    // B's own rollout appears and is held by B's pid → binds with A alive.
    let later = now.format(&Rfc3339).unwrap();
    write_rollout_prompt_at(home.path(), "B-second", cwd, &later, "B prompt words");
    let b_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-B-second.jsonl");
    probe_b.hold(&b_path, 2002);
    assert!(b.discover().unwrap());
    assert_eq!(b.binding().map(|b| b.session_id.as_str()), Some("B-second"));
}

/// A binds and STAYS OPEN; B must still bind its own rollout (A's window was
/// released on bind; A's prompt authorizes only A).
#[test]
fn a_still_open_idle_while_b_binds_its_own_rollout() {
    // r8 item 1: A stays ALIVE; B binds its own process-held file via tier b.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r8-open";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);

    let (probe_a, roots_a) = fake_ownership(1001);
    let (probe_b, roots_b) = fake_ownership(2002);

    write_rollout_prompt_at(
        home.path(),
        "A-open",
        cwd,
        &now.format(&Rfc3339).unwrap(),
        "fixture driver prompt",
    );
    let a_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-A-open.jsonl");
    probe_a.hold(&a_path, 1001);
    let mut a =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_A, 1001, &probe_a, &roots_a);
    assert!(a.discover().unwrap());

    // B launches while A is still live; A's claimed file is excluded and B's
    // probe holds no fd on it.
    let mut b =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_B, 2002, &probe_b, &roots_b);
    assert!(!b.discover().unwrap(), "B cannot touch A's bound file");
    write_rollout_prompt_at(
        home.path(),
        "B-open",
        cwd,
        &now.format(&Rfc3339).unwrap(),
        "fixture driver prompt",
    );
    let b_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-B-open.jsonl");
    probe_b.hold(&b_path, 2002);
    assert!(b.discover().unwrap(), "B binds its own process-held file");
    assert_eq!(b.binding().map(|b| b.session_id.as_str()), Some("B-open"));
}

/// After A closes, an idle B must not rebind A's file even though A's prompt
/// remains in the per-cwd history.
#[test]
fn a_closed_owner_file_is_never_rebound_by_idle_b() {
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r6-close";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);

    send_fixture_prompt(Path::new(cwd), INST_A, "fixture driver prompt", now);
    let mut a = launch_adapter_keyed(home.path(), cwd, floor, INST_A);
    write_rollout_prompt_at(
        home.path(),
        "A-closed",
        cwd,
        &now.format(&Rfc3339).unwrap(),
        "fixture driver prompt",
    );
    assert!(a.discover().unwrap());
    let path = a.binding().unwrap().main_file.clone().unwrap();
    assert!(crate::adapters::codex_discovery::claimed_owner_of(&path).is_some());
    drop(a);
    // Claim survives the close, keyed to A.
    assert_eq!(
        crate::adapters::codex_discovery::claimed_owner_of(&path).as_deref(),
        Some(INST_A)
    );

    let mut b = launch_adapter_keyed(home.path(), cwd, floor, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, "fixture driver prompt", now);
    for _ in 0..3 {
        assert!(
            !b.discover().unwrap(),
            "B never re-binds the closed owner's file"
        );
        assert!(b.binding().is_none());
    }
}

// c-usagefu r9 item 1: lifetime intervals, not discoverer registration ----

#[test]
fn a_rollout_created_by_an_instance_closed_before_b_launches_stays_unbound() {
    // The exact r9 review sequence:
    //   A sends "continue", creates rollout F at t0, and CLOSES before ever
    //   claiming it;
    //   B launches in the same cwd at t0+2s and sends "continue" too.
    // F passes the locator's 5 s launch slack (B's floor is t0+2) and the
    // prompt pre-send slack, so r8 prompt attribution let B steal A's file.
    // r9 item 1: prompt-only attribution requires the file created at/after
    // B's ACTUAL launch — F never binds to B. B's OWN later file G (B sole
    // live over [launch, creation]) binds normally.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r9-item1-closed-before-launch";
    let now = OffsetDateTime::now_utc();
    let t0 = now - time::Duration::seconds(3);

    // A launches, sends "continue", creates F at t0, never scans, then
    // closes at t0+1 (before B launches at t0+2).
    let a = launch_adapter_keyed(home.path(), cwd, t0, INST_A);
    send_fixture_prompt(Path::new(cwd), INST_A, "continue", t0);
    write_rollout_prompt_at(
        home.path(),
        "F-closed",
        cwd,
        &t0.format(&Rfc3339).unwrap(),
        "continue",
    );
    let f_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-F-closed.jsonl");
    a.close_lifetime_at(t0 + time::Duration::seconds(1));
    drop(a);
    assert_eq!(
        crate::adapters::codex_discovery::claimed_owner_of(&f_path),
        None,
        "A closed before claiming F: F is unclaimed"
    );

    // B launches at t0+2 and sends the identical prompt.
    let b_floor = t0 + time::Duration::seconds(2);
    let mut b = launch_adapter_keyed(home.path(), cwd, b_floor, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, "continue", now);
    for _ in 0..3 {
        assert!(
            !b.discover().unwrap(),
            "F was created before B launched: prompt text must never bind it"
        );
        assert!(
            b.binding().is_none(),
            "F stays unbound for B despite both slacks"
        );
        assert_eq!(
            crate::adapters::codex_discovery::claimed_owner_of(&f_path),
            None,
            "the rejected scans leave F unclaimed"
        );
    }

    // B's own session G is created while B is the sole live instance in the
    // cwd (A closed before B launched): prompt attribution binds G.
    let g_at = b_floor + time::Duration::seconds(1);
    write_rollout_prompt_at(
        home.path(),
        "G-own",
        cwd,
        &g_at.format(&Rfc3339).unwrap(),
        "continue",
    );
    assert!(
        b.discover().unwrap(),
        "B binds its own post-launch rollout G"
    );
    assert_eq!(
        b.binding().map(|binding| binding.session_id.as_str()),
        Some("G-own")
    );
    assert_eq!(
        crate::adapters::codex_discovery::claimed_owner_of(&f_path),
        None,
        "F remains unclaimed"
    );
}

// c-usagefu r10 item 1 (Ruling A): a promoted (unknown launch boundary) codex
// in the cwd makes the census incomplete, so prompt-only attribution is
// refused even for a generic launch that otherwise looks like the sole match.

#[test]
fn a_promoted_unknown_boundary_codex_blocks_generic_prompt_attribution() {
    // The exact r10 review sequence:
    //   a promoted Codex A (operator-started, launch boundary unknown) and a
    //   generic driver B share one cwd;
    //   both dispatch "continue";
    //   A's process creates rollout F AFTER B launched, but B scans BEFORE A
    //   ever claims it.
    // Without the promoted lifetime the r9 census saw only B and let it steal
    // A's rollout. r10 item 1 registers A with an UNKNOWN boundary: the census
    // is incomplete -> prompt-only is refused in the cwd, F stays unclaimed.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r10-item1-census";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let rfc = now.format(&Rfc3339).unwrap();

    // The promoted watch registers A's unknown-boundary lifetime at detection.
    let promoted_a = crate::adapters::codex_discovery::begin_lifetime(
        Path::new(cwd),
        "promoted-codex-4242",
        None,
    );

    // A and B both send the identical prompt; F is created after B's launch.
    send_fixture_prompt(Path::new(cwd), "promoted-codex-4242", "continue", now);
    let mut b = launch_adapter_keyed(home.path(), cwd, floor, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, "continue", now);
    write_rollout_prompt_at(home.path(), "F-promoted", cwd, &rfc, "continue");
    let f_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-F-promoted.jsonl");

    assert!(
        !crate::adapters::codex_discovery::sole_instance_matches(
            Path::new(cwd),
            INST_B,
            "continue",
            now
        ),
        "the unknown-boundary A makes B's sole-instance proof unprovable"
    );
    for _ in 0..3 {
        assert!(
            !b.discover().unwrap(),
            "B must not prompt-bind the promoted instance's rollout"
        );
        assert!(b.binding().is_none(), "F stays unbound for B");
    }
    assert_eq!(
        crate::adapters::codex_discovery::claimed_owner_of(&f_path),
        None,
        "the refused scans leave F unclaimed"
    );

    // A promoted instance itself never uses prompt-only attribution (its floor
    // is unknown), regardless of how clean the rest of the census looks.
    assert!(
        !crate::adapters::codex_discovery::sole_instance_matches(
            Path::new(cwd),
            "promoted-codex-4242",
            "continue",
            now
        ),
        "an unknown-boundary owner is never a prompt-only attributor"
    );
    drop(promoted_a);
}

#[test]
fn a_known_boundary_native_codex_overlapping_blocks_generic_prompt_attribution() {
    // r10 item 1 companion: when the OTHER same-cwd codex is a native launch
    // with a KNOWN boundary that overlaps [B launch, F creation], B is refused
    // just the same — the fix is not specific to the unknown case.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r10-item1-known-overlap";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let rfc = now.format(&Rfc3339).unwrap();

    // Native codex A launched before B, still live when F is created.
    let native_a = crate::adapters::codex_discovery::begin_lifetime(
        Path::new(cwd),
        "native-A",
        Some(now - time::Duration::seconds(20)),
    );
    send_fixture_prompt(Path::new(cwd), "native-A", "continue", now);
    let mut b = launch_adapter_keyed(home.path(), cwd, floor, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, "continue", now);
    write_rollout_prompt_at(home.path(), "F-native", cwd, &rfc, "continue");
    let f_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-F-native.jsonl");

    for _ in 0..3 {
        assert!(
            !b.discover().unwrap(),
            "B must not prompt-bind while native A overlaps"
        );
        assert!(b.binding().is_none());
    }
    assert_eq!(
        crate::adapters::codex_discovery::claimed_owner_of(&f_path),
        None
    );
    drop(native_a);
}

#[test]
fn a_rejected_candidate_is_never_rematched_even_after_the_blocker_leaves_the_census() {
    // c-usagefu r11 item 1 (Ruling A1, EXACT):
    //   A and B overlap in one cwd; B sends P;
    //   A creates F carrying P AFTER B's launch, then closes;
    //   B scans while A's closed interval is retained -> B rejects F.
    // The rejection is stored PERMANENTLY against (B, F): once issued, nothing
    // — the blocker merely closing, its interval being pruned past any horizon,
    // or the blocker vanishing from the census entirely — may ever turn F into
    // a match for B. (r10 item 2 wrongly let F bind B after a one-hour prune:
    // that directly contradicted Ruling A.)
    use crate::adapters::codex_discovery::LifetimeGuardClock as _;

    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r11-item1-permanent";
    let base = OffsetDateTime::now_utc() - time::Duration::seconds(40);
    let a_launch = base;
    let b_floor = base + time::Duration::seconds(10);
    let f_at = b_floor + time::Duration::seconds(5);
    let a_close = f_at + time::Duration::seconds(1);
    let prompt = "shared rollout prompt P";

    // A registers before B, sends the same prompt, then creates F after B's
    // launch and closes (retained, close after B's floor -> overlap).
    let a =
        crate::adapters::codex_discovery::begin_lifetime(Path::new(cwd), INST_A, Some(a_launch));
    send_fixture_prompt(Path::new(cwd), INST_A, prompt, b_floor);

    let mut b = launch_adapter_keyed(home.path(), cwd, b_floor, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, prompt, b_floor);
    write_rollout_prompt_at(
        home.path(),
        "F-r11-permanent",
        cwd,
        &f_at.format(&Rfc3339).unwrap(),
        prompt,
    );
    let f_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-F-r11-permanent.jsonl");

    a.close_lifetime_at(a_close);
    drop(a);

    // First scan: A's interval overlaps -> B rejects F.
    assert!(!b.discover().unwrap(), "B rejects F while A overlaps");
    assert!(b.binding().is_none(), "F stays unbound for B");

    // The blocker leaves the census ENTIRELY — Ruling A1 has no expiry: not on
    // blocker close, not on interval pruning, not on any census change.
    crate::adapters::codex_discovery::forget_lifetime(Path::new(cwd), INST_A);

    // A clean re-judgment with NO blocker on the census would match — the
    // permanent rejection is the only thing standing between B and F now.
    assert!(
        crate::adapters::codex_discovery::sole_instance_matches(
            Path::new(cwd),
            INST_B,
            prompt,
            f_at
        ),
        "control: with the rejection memory aside, the census alone looks sole"
    );
    for _ in 0..3 {
        assert!(
            !b.discover().unwrap(),
            "the production path keeps rejecting F long after A left the census"
        );
        assert!(b.binding().is_none(), "F never binds B");
    }
    assert_eq!(
        crate::adapters::codex_discovery::claimed_owner_of(&f_path),
        None,
        "F is still unclaimed"
    );
}

#[test]
fn a_closed_interval_is_retained_for_a_pending_decision_past_any_hour_boundary() {
    // c-usagefu r11 item 5 (Ruling A2 exact): closed intervals are retained
    // while ANY pending discovery decision could overlap them — there is no
    // fixed one-hour prune. r10 pruned every closed segment older than an
    // hour on the next same-cwd registration, so a live unbound discoverer
    // scanning lazily more than an hour later saw an incomplete census and
    // could bind a rollout the closed instance actually owned. Once the
    // cwd's LAST discovery window closes (no decision remains pending), the
    // closed segments are dropped and live intervals kept.
    use crate::adapters::codex_discovery::LifetimeGuardClock as _;

    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r11-item5-retention";
    // Anchor near wall-clock now (concurrent real-time tests share the prompt
    // registry's age sweep), then walk TWO HOURS forward.
    let base = OffsetDateTime::now_utc() - time::Duration::seconds(20);
    let b_floor = base + time::Duration::seconds(5);
    let f_at = base + time::Duration::seconds(7);
    let a_close = base + time::Duration::seconds(10);
    let prompt = "late lazy rollout prompt";

    // A registers, shares the window with B over F's creation, then closes —
    // while B is still a live unbound discoverer.
    let a = crate::adapters::codex_discovery::begin_lifetime(Path::new(cwd), INST_A, Some(base));
    let mut b = launch_adapter_keyed(home.path(), cwd, b_floor, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, prompt, b_floor);
    write_rollout_prompt_at(
        home.path(),
        "F-retained",
        cwd,
        &f_at.format(&Rfc3339).unwrap(),
        prompt,
    );
    a.close_lifetime_at(a_close);
    drop(a);

    // While A's close is recent the census proof is unprovable: B refuses F.
    assert!(
        !crate::adapters::codex_discovery::sole_instance_matches(
            Path::new(cwd),
            INST_B,
            prompt,
            f_at
        ),
        "the retained closed segment keeps the census sole-live proof unprovable"
    );
    assert!(
        !b.discover().unwrap(),
        "the production discovery path refuses F"
    );
    assert!(b.binding().is_none());

    // Wall-clock advances past the old one-hour horizon WHILE B is still a
    // live unbound discoverer: move A's recorded close two hours into the
    // past, then another same-cwd instance registers (r10 pruned on exactly
    // this). The pending-decision rule retains A regardless of age; r10's
    // fixed-hour prune discarded it here.
    crate::adapters::codex_discovery::retroactively_close_at(
        Path::new(cwd),
        INST_A,
        OffsetDateTime::now_utc() - time::Duration::hours(2) - time::Duration::minutes(10),
    );
    let _c = crate::adapters::codex_discovery::begin_lifetime(Path::new(cwd), INST_C, None);
    assert!(
        crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_A),
        "A's closed segment is retained two hours past close while B's decision is pending"
    );
    // The production path keeps refusing F for B too (the Ruling A1 permanent
    // rejection from the first judgment).
    assert!(!b.discover().unwrap(), "B still refuses F two hours on");
    assert!(b.binding().is_none());

    // B gives up: its discovery window is the cwd's last one, so on close the
    // pending-decision retention ends — A's CLOSED segment is dropped while
    // C's LIVE interval stays.
    drop(b);
    assert!(
        !crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_A),
        "closed segments are pruned when the last window closes, not by the clock"
    );
    assert!(
        crate::adapters::codex_discovery::census_lifetime_is_live(Path::new(cwd), INST_C),
        "a live interval survives the closed-segment prune"
    );
}

#[test]
fn a_pending_launch_window_survives_another_window_releasing_before_adapter_build() {
    // c-usagefu r12 item 2 (Ruling A2'): a generic-pty codex launch registers
    // its pending DISCOVERY window TOGETHER with its lifetime BEFORE the
    // process starts. The file adapter is constructed only after agent
    // settlement (up to 60 s later); r11 opened the window only at adapter
    // construction, so another instance's window releasing during that gap was
    // seen as the cwd's LAST window and pruned every closed interval a
    // registered-but-not-yet-adapted launch's later decision still needed.
    //
    // Timeline (all anchored to wall-clock now so concurrent prompt sweeps do
    // not evict the evidence):
    //   t0    A launches (window open) and creates FA at t0.5
    //   t1    C launches (known boundary)
    //   t2    B registers lifetime + pending window BEFORE its process — no
    //         adapter yet; B dispatches PB
    //   t2.2  C closes
    //   t2.5  A binds FA: its window releases. B's pending window keeps the
    //         cwd non-last, so C's just-closed interval is retained.
    //   t5    B's adapter is built (handed the SAME window) and FB appears.
    //         C overlaps [B launch t2, creation t5] (closed t2.2 > t2) so B
    //         must REFUSE FB. Under r11 the last-window prune at t2.5 dropped
    //         C, B looked sole, and bound a rollout it did not own.
    use crate::adapters::codex_discovery::LifetimeGuardClock as _;

    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r12-item2-pending";
    let t0 = OffsetDateTime::now_utc() - time::Duration::seconds(30);
    let t1 = t0 + time::Duration::seconds(1);
    let t2 = t0 + time::Duration::seconds(2);
    let c_close = t0 + time::Duration::milliseconds(2200);
    let t5 = t0 + time::Duration::seconds(5);

    // A launches first; FA exists before any other instance.
    let mut a = launch_adapter_keyed(home.path(), cwd, t0, INST_A);
    send_fixture_prompt(Path::new(cwd), INST_A, "A prompt PA", t0);
    write_rollout_prompt_at(
        home.path(),
        "FA-r12-pending",
        cwd,
        &(t0 + time::Duration::milliseconds(500))
            .format(&Rfc3339)
            .unwrap(),
        "A prompt PA",
    );

    // C launches with a known boundary.
    let c_guard =
        crate::adapters::codex_discovery::begin_lifetime(Path::new(cwd), INST_C, Some(t1));

    // B registers its lifetime AND its pending discovery window as the one
    // pre-launch bundle BEFORE the process starts; the adapter is built only
    // much further below. The boundary is sampled inside the registration
    // critical section (r14 L1); pin it to t2 for this deterministic timeline.
    crate::adapters::codex_discovery::set_test_launch_boundary(Path::new(cwd), t2);
    let b_pending =
        crate::adapters::codex_discovery::register_pending_launch(Path::new(cwd), INST_B, None);
    let t_boundary = b_pending.launched_at;
    let b_lifetime = b_pending.lifetime;
    let b_window = b_pending.window;
    send_fixture_prompt(
        Path::new(cwd),
        INST_B,
        "B prompt PB",
        t2 + time::Duration::seconds(1),
    );

    // C closes AFTER B registered but BEFORE A's window releases.
    c_guard.close_lifetime_at(c_close);
    drop(c_guard);

    // A binds its own rollout: census over [t0, FA creation t0.5] contains
    // neither C (launched t1) nor B (t2), so A is sole and binds. Its window
    // releases — but B's pending window keeps that from being the cwd's last.
    assert!(a.discover().unwrap(), "A binds its own earlier rollout");
    assert!(a.binding().is_some());
    assert!(
        crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_C),
        "C's closed interval survives A's release while B is registered-but-pending"
    );

    // FB appears at t5; only NOW is B's adapter constructed, taking the SAME
    // pre-registered window (exactly the generic-pty wiring's handover).
    write_rollout_prompt_at(
        home.path(),
        "FB-r12-pending",
        cwd,
        &t5.format(&Rfc3339).unwrap(),
        "B prompt PB",
    );
    let mut b = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: PathBuf::from(cwd),
        pid: None,
        launched_at: Some(t_boundary),
        resume_session_id: None,
    })
    .with_lifetime(b_lifetime.witness())
    .with_window(b_window)
    .with_instance_key(Some(INST_B.to_owned()));

    // Production discovery: C's retained closed interval overlaps B's census
    // window, so prompt-only attribution is refused.
    assert!(
        !b.discover().unwrap(),
        "B must not bind FB: the pruned-on-last-window interval C still overlaps"
    );
    assert!(b.binding().is_none());

    // B gives up: its pre-registered window was the cwd's last one, so closing
    // it ends retention — C's CLOSED segment is pruned, B's own live interval
    // stays.
    drop(b);
    assert!(
        !crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_C),
        "closed segments prune once the last (pending) window closes"
    );
    assert!(
        crate::adapters::codex_discovery::census_lifetime_is_live(Path::new(cwd), INST_B),
        "B's live interval is retained"
    );
}

#[test]
fn a_retained_stop_incomplete_guard_dropped_without_a_confirmed_exit_stays_live() {
    // c-usagefu r17 Ruling W0: a guard retained for a stop-incomplete teardown
    // is closed ONLY by an explicit confirmed-exit close. When Node shutdown
    // drains the retention bag, dropping the owner guard must NOT record a
    // close — a false close is worse than an interval that stays live until
    // the Node restarts. An explicit close before the drop is still recorded
    // exactly once.
    use crate::adapters::codex_discovery::LifetimeGuardClock as _;

    let cwd = "/projects/r13-item3-drop";
    let guard = crate::adapters::codex_discovery::begin_lifetime(
        Path::new(cwd),
        "abandoned-owner",
        Some(OffsetDateTime::now_utc()),
    );
    guard.suppress_drop_close();
    let closed_at = OffsetDateTime::now_utc() + time::Duration::seconds(5);
    guard.close_lifetime_at(closed_at);
    drop(guard);
    assert!(
        crate::adapters::codex_discovery::census_lifetime_is_live(
            Path::new(cwd),
            "abandoned-owner"
        ),
        "Drop after suppress_drop_close records no close even with a test clock set"
    );

    // The explicit confirmed-exit path still closes, and only once.
    let guard2 = crate::adapters::codex_discovery::begin_lifetime(
        Path::new(cwd),
        "confirmed-owner",
        Some(OffsetDateTime::now_utc()),
    );
    guard2.suppress_drop_close();
    guard2.close_lifetime_at(closed_at);
    guard2.close_interval();
    assert!(
        !crate::adapters::codex_discovery::census_lifetime_is_live(
            Path::new(cwd),
            "confirmed-owner"
        ),
        "confirmed exit closes the interval despite drop suppression"
    );
    drop(guard2);
}

#[test]
fn a_window_releasing_inside_a_pending_launch_registration_cannot_prune_its_interval() {
    // c-usagefu r13 item 1: register_pending_launch must register the launch
    // boundary, the lifetime AND the window retention as ONE critical section
    // with respect to last-window pruning. r12 did the lifetime insert and the
    // window open under two separate mutexes, so another window's release
    // landing in that gap was still seen as the cwd's LAST window: it pruned
    // C's just-closed interval, and B's later real discover() bound C's
    // rollout although C overlapped B's whole [launch, creation] window.
    //
    // Barrier-controlled timeline:
    //   t0      A launches (its window is the cwd's only one)
    //   t0+1    C launches with a known boundary
    //   t0+2    B registers: lifetime inserted, thread parks BEFORE its
    //           window opens, still holding the registration critical section
    //   t0+2.5  C's rollout FC is created (this stamp is written later)
    //   t0+3    C closes — AFTER B's boundary and AFTER FC's creation
    //   in-gap  A's window releases (A's own interval closed at t0-1, so it is
    //           deliberately not a blocker) while B is parked
    //   then    B's window opens; B runs the REAL discover() path.
    use crate::adapters::codex_discovery::{
        LifetimeGuardClock as _, PendingLaunchBarrierHandle as PendingLaunchBarrier,
    };

    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r13-item1-atomic";
    let t0 = OffsetDateTime::now_utc() - time::Duration::seconds(10);
    let t_c = t0 + time::Duration::seconds(1);
    let t_b = t0 + time::Duration::seconds(2);
    let t_f = t0 + time::Duration::milliseconds(2500);
    let t_c_close = t0 + time::Duration::seconds(3);

    // A: the window that will release inside B's registration. Its OWN
    // interval is closed before B's boundary so it can never be the blocker;
    // only the retention prune is on trial here.
    let a = launch_adapter_keyed(home.path(), cwd, t0, INST_A);
    a.close_lifetime_at(t0 - time::Duration::seconds(1));

    // C launches with a known boundary; it closes after B's boundary.
    let c_guard =
        crate::adapters::codex_discovery::begin_lifetime(Path::new(cwd), INST_C, Some(t_c));
    c_guard.close_lifetime_at(t_c_close);
    drop(c_guard);
    assert!(
        crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_C),
        "C's closed interval is retained while A's window is open"
    );

    crate::adapters::codex_discovery::set_test_launch_boundary(Path::new(cwd), t_b);
    let barrier = PendingLaunchBarrier::arm(Path::new(cwd));
    let (tx, rx) = std::sync::mpsc::channel();
    let builder = std::thread::Builder::new()
        .name("r13-item1-pending-launch".into())
        .spawn(move || {
            let pending = crate::adapters::codex_discovery::register_pending_launch(
                Path::new(cwd),
                INST_B,
                None,
            );
            tx.send(pending).unwrap();
        })
        .unwrap();

    barrier.wait_to_enter();
    assert!(
        crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_B),
        "B's lifetime is registered before the window opens"
    );

    // A releases its window INSIDE B's registration. With the r12 code this
    // prunes C (B has no window yet, so A looks like the last window); with
    // the atomic registration the release blocks until B's window is open and
    // then only decrements the count 2 -> 1.
    //
    // c-usagefu r14 item 6: prove deterministically that the releaser is
    // queued on LAUNCH_REGISTRATION BEFORE B is allowed to proceed — a fixed
    // sleep could release B before the releaser even reached the lock.
    let release_blocked =
        crate::adapters::codex_discovery::release_blocked_receiver(Path::new(cwd));
    let releaser = std::thread::Builder::new()
        .name("r13-item1-release".into())
        .spawn(move || drop(a))
        .unwrap();
    release_blocked
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the releaser is blocked on LAUNCH_REGISTRATION before B is released");

    barrier.release();
    let b_pending = rx.recv().unwrap();
    builder.join().unwrap();
    releaser.join().unwrap();

    assert!(
        crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_C),
        "C's closed interval survives a release that raced B's registration"
    );

    // FC appears after B's boundary but while C was still live; B dispatched
    // the matching prompt.
    let prompt = "B item1 prompt";
    send_fixture_prompt(
        Path::new(cwd),
        INST_B,
        prompt,
        t0 + time::Duration::seconds(3),
    );
    write_rollout_prompt_at(
        home.path(),
        "FC-r13-item1",
        cwd,
        &t_f.format(&Rfc3339).unwrap(),
        prompt,
    );

    // The real production discovery path — no test-only census predicate:
    // C's retained interval overlaps [B launch, FC creation], so B refuses.
    let b_lifetime = b_pending.lifetime;
    let b_window = b_pending.window;
    let mut b = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: PathBuf::from(cwd),
        pid: None,
        launched_at: Some(t_b),
        resume_session_id: None,
    })
    .with_lifetime(b_lifetime.witness())
    .with_window(b_window)
    .with_instance_key(Some(INST_B.to_owned()));
    assert!(
        !b.discover().unwrap(),
        "B must not bind FC: the interval pruned inside its registration still overlaps"
    );
    assert!(b.binding().is_none());
}

#[test]
fn a_launch_blocked_before_the_lock_samples_its_boundary_after_a_close_and_refuses_the_rollout() {
    // c-usagefu r14 item 2 (Ruling L1): the production launch boundary is
    // sampled INSIDE the LAUNCH_REGISTRATION critical section together with
    // the lifetime insert and window retention, and THAT boundary is returned
    // to the adapter.
    //
    // Timeline:
    //   A's window is the cwd's only one (A's own interval long closed).
    //   C launched earlier, still live.
    //   B decides to launch and registers, but is parked BEFORE taking
    //      LAUNCH_REGISTRATION — it holds neither a lifetime nor a window.
    //   while B is parked: FC (B's matching prompt) is written and C closes.
    //   A's deferred cleanup drops its window: with B holding nothing it is
    //      genuinely the last window, so C's closed interval is pruned.
    //   only now does B take the lock and sample its boundary — AFTER FC and
    //      after C's close.
    //
    // With a caller-sampled boundary (the pre-L1 shape), B's floor predated
    // FC: the prune had removed C, B was sole-live, and the matching prompt
    // bound a rollout created while a since-closed instance was actually
    // running. B's REAL discover() must now refuse FC on the strict resume
    // floor.
    use crate::adapters::codex_discovery::{
        LifetimeGuardClock as _, PendingLaunchBarrierHandle as PendingLaunchBarrier,
    };

    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r14-item2-boundary";
    let prompt = "B r14 item2 launch prompt";
    let rfc = |at: OffsetDateTime| at.format(&Rfc3339).unwrap();

    // A: the window whose release lands while B is parked pre-lock. A's own
    // interval closed in the past, so it can never be the census blocker.
    let a = launch_adapter_keyed(
        home.path(),
        cwd,
        OffsetDateTime::now_utc() - time::Duration::seconds(30),
        INST_A,
    );
    a.close_lifetime_at(OffsetDateTime::now_utc() - time::Duration::seconds(20));

    // C launched earlier, still live while B decides. c-usagefu r15 item 8(b):
    // C's lifetime comes through the PRODUCTION confirmed-launch registration
    // (the same path a hook-shadowed codex spawn takes), never a hand-built
    // guard — its boundary is whatever the production path samples.
    let (c_guard, _c_boundary) =
        crate::adapters::codex_discovery::register_confirmed_launch(Path::new(cwd), INST_C, None);

    // B registers on another thread and parks before the lock.
    let barrier = PendingLaunchBarrier::arm_pre_lock(Path::new(cwd));
    let (tx, rx) = std::sync::mpsc::channel();
    let builder = std::thread::Builder::new()
        .name("r14-item2-pending-launch".into())
        .spawn(move || {
            let pending = crate::adapters::codex_discovery::register_pending_launch(
                Path::new(cwd),
                INST_B,
                None,
            );
            tx.send(pending).unwrap();
        })
        .unwrap();
    barrier.wait_before_lock();
    assert!(
        !crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_B),
        "parked before the lock: B has registered no lifetime yet"
    );

    // FC is written AFTER B decided to launch, then C closes. Whole-second
    // RFC3339 timestamps bound both sides of the ordering: sleep long enough
    // that FC cannot share B's decision second...
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let fc_started = OffsetDateTime::now_utc();
    write_rollout_prompt_at(home.path(), "FC-r14-item2", cwd, &rfc(fc_started), prompt);
    c_guard.close_lifetime_at(OffsetDateTime::now_utc());
    drop(c_guard);

    // A's last-window release lands while B holds nothing: it prunes C and
    // clears prompt evidence, exactly the r13 race. Joined deterministically.
    let releaser = std::thread::Builder::new()
        .name("r14-item2-release".into())
        .spawn(move || drop(a))
        .unwrap();
    releaser.join().unwrap();
    assert!(
        !crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_C),
        "A really released as the last window and pruned C"
    );

    // ...and release B far enough past FC's timestamp second that the boundary
    // it samples inside the lock is strictly AFTER FC and after C's close.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    barrier.release_pre_lock();
    let b_pending = rx.recv().unwrap();
    builder.join().unwrap();
    assert!(
        crate::adapters::codex_discovery::census_contains(Path::new(cwd), INST_B),
        "B registered its lifetime once it took the lock"
    );

    // B dispatched the matching prompt only after registering (the last-window
    // release cleared the cwd's earlier evidence).
    send_fixture_prompt(Path::new(cwd), INST_B, prompt, OffsetDateTime::now_utc());

    // The adapter receives the EXACT boundary registration returned.
    let b_lifetime = b_pending.lifetime;
    let b_window = b_pending.window;
    let mut b = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: PathBuf::from(cwd),
        pid: None,
        launched_at: Some(b_pending.launched_at),
        resume_session_id: None,
    })
    .with_lifetime(b_lifetime.witness())
    .with_window(b_window)
    .with_instance_key(Some(INST_B.to_owned()));
    assert!(
        !b.discover().unwrap(),
        "B must not bind FC: its in-lock boundary is after FC's creation and C's close"
    );
    assert!(b.binding().is_none());
    assert!(
        b_pending.launched_at > fc_started,
        "the returned boundary is the in-lock sample after FC's creation"
    );
}

#[test]
fn arming_a_second_barrier_disarms_the_first_and_releases_its_parked_registration() {
    // c-usagefu r14 item 6: arm disarms a PREVIOUS barrier first. A
    // registration parked on the old barrier must be released by the new arm
    // itself, without anyone calling the old barrier's release(). A stale arm
    // that merely overwrote the slot left the thread parked forever.
    use crate::adapters::codex_discovery::PendingLaunchBarrierHandle as PendingLaunchBarrier;

    let cwd = "/projects/r14-item6-disarm";
    let (tx, rx) = std::sync::mpsc::channel();
    let first = PendingLaunchBarrier::arm_pre_lock(Path::new(cwd));
    let builder = std::thread::Builder::new()
        .name("r14-item6-old-registration".into())
        .spawn(move || {
            let pending = crate::adapters::codex_discovery::register_pending_launch(
                Path::new(cwd),
                INST_B,
                None,
            );
            tx.send(pending).unwrap();
        })
        .unwrap();
    first.wait_before_lock();

    // A newer arm for the SAME cwd disarms `first`; its parked registration
    // runs to completion on its own (the new barrier has the between stage
    // disabled).
    let second = PendingLaunchBarrier::arm_pre_lock(Path::new(cwd));
    let pending = rx
        .recv_timeout(std::time::Duration::from_millis(2000))
        .expect("arming a newer barrier releases a registration parked on the old one");
    builder.join().unwrap();
    drop(pending);
    drop(second);
    drop(first);
}

#[test]
fn dropping_a_replaced_barrier_never_clears_the_currently_armed_barrier() {
    // c-usagefu r14 item 6: Drop clears the static slot ONLY while it still
    // holds THIS barrier (Arc identity). After a second arm replaced the
    // first, dropping the first Arc must leave the new barrier armed — a
    // later registration parks on it. Unconditional clearing made a later
    // test's registration sail past an armed barrier.
    use crate::adapters::codex_discovery::PendingLaunchBarrierHandle as PendingLaunchBarrier;

    let cwd = "/projects/r14-item6-ptr-eq";
    let first = PendingLaunchBarrier::arm_pre_lock(Path::new(cwd));
    let second = PendingLaunchBarrier::arm_pre_lock(Path::new(cwd));
    // `first` is no longer the armed barrier; its Drop must not clear `second`.
    drop(first);

    let (tx, rx) = std::sync::mpsc::channel();
    let builder = std::thread::Builder::new()
        .name("r14-item6-parked-registration".into())
        .spawn(move || {
            let pending = crate::adapters::codex_discovery::register_pending_launch(
                Path::new(cwd),
                INST_B,
                None,
            );
            tx.send(pending).unwrap();
        })
        .unwrap();
    // No blocking wait: with the entry intact the registration parks (nothing
    // is sent); if the replaced barrier's Drop cleared the current barrier the
    // registration sails through and this receipt succeeds.
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(500))
            .is_err(),
        "the registration stays parked on the still-armed second barrier"
    );
    second.release_pre_lock();
    let pending = rx
        .recv_timeout(std::time::Duration::from_millis(2000))
        .unwrap();
    builder.join().unwrap();
    drop(pending);
}

#[test]
fn a_resumed_instance_discovers_through_real_adapters_without_bridging_the_gap() {
    // c-usagefu r12 item 6 (Ruling A2): the r11 version of this regression
    // hand-built lifetime segments and judged through the test-only
    // sole_instance_matches seam. The real guarantees are now exercised on
    // the production discovery path with ACTUAL rollout fixtures:
    //
    //   A's first interval runs t0..t20, then CLOSES;
    //   a GAP (no A process) runs t20..t100, F_gap is created at t50;
    //   C launches at t30 in the same cwd and binds F_gap at t50 — A's
    //   retained closed segment must not invent an overlap across the gap;
    //   A RESUMES at t100 (stable instance key, a NEW interval): a REAL
    //   CodexAdapter with the resume floor must NOT bind F_gap (created in
    //   the closed gap), while a rollout F_after created at t107 binds.
    //
    // Times are anchored close to wall-clock now: concurrent real-time tests
    // share the process prompt registry and its age sweep.
    use crate::adapters::codex_discovery::LifetimeGuardClock as _;

    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r12-item6-resume";
    let t0 = OffsetDateTime::now_utc() - time::Duration::seconds(120);
    let t20 = t0 + time::Duration::seconds(20);
    let t30 = t0 + time::Duration::seconds(30);
    let t45 = t0 + time::Duration::seconds(45);
    let t50 = t0 + time::Duration::seconds(50);
    let t90 = t0 + time::Duration::seconds(90);
    let t100 = t0 + time::Duration::seconds(100);
    let t105 = t0 + time::Duration::seconds(105);
    let t107 = t0 + time::Duration::seconds(107);

    // A's first cohort: registered at t0, confirmed closed at t20 (the driver
    // closes only after the stop ladder; here the test clock places it).
    let first = crate::adapters::codex_discovery::begin_lifetime(Path::new(cwd), INST_A, Some(t0));
    first.close_lifetime_at(t20);
    drop(first);

    // F_gap is written DURING THE CLOSED GAP at t50.
    write_rollout_prompt_at(
        home.path(),
        "F-gap",
        cwd,
        &t50.format(&Rfc3339).unwrap(),
        "gap prompt",
    );

    // The resume cohort is registered on the STABLE instance key with its
    // t100 floor. It is created before C judges below on purpose: the census
    // is pure interval math over [launch, creation], and the invariant is that
    // a resume APPENDS a segment at t100 without REOPENING the closed t0
    // segment — so even with the resumed cohort on record, nothing of A's
    // reaches back over the gap. The r10 shape reopened one t0..live record
    // on resume and blocked C across the gap.
    let resumed =
        crate::adapters::codex_discovery::begin_lifetime(Path::new(cwd), INST_A, Some(t100));

    // C launches at t30 and runs the REAL discovery path: its prompt was
    // dispatched at t45 and F_gap was created at t50. A's retained segment
    // closed at t20 — before C launched — and the resumed segment starts only
    // at t100 (after F_gap was created), so C is sole and binds.
    let mut c = launch_adapter_keyed(home.path(), cwd, t30, INST_C);
    send_fixture_prompt(Path::new(cwd), INST_C, "gap prompt", t45);
    assert!(
        c.discover().unwrap(),
        "production discovery: C binds the gap rollout with no bridged blocker"
    );
    assert_eq!(c.binding().map(|b| b.session_id.as_str()), Some("F-gap"));
    // C closes at t90 — still before A's resume interval begins at t100.
    c.close_lifetime_at(t90);
    drop(c);

    // The resumed session gets a REAL adapter whose floor is the resume time,
    // handed a witness to the pre-spawned lifetime exactly as the shell-pty
    // resume wiring does.
    let mut a_resumed = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: PathBuf::from(cwd),
        pid: None,
        launched_at: Some(t100),
        resume_session_id: None,
    })
    .with_lifetime(resumed.witness())
    .with_instance_key(Some(INST_A.to_owned()));

    // F_gap is in the resume home but the locator's floor is the resume time:
    // it is NOT returned, so the closed-gap rollout can never bind.
    assert!(
        !a_resumed.discover().unwrap(),
        "a rollout created in the closed gap is pre-launch for the resume"
    );
    assert!(a_resumed.binding().is_none());

    // A post-resume rollout binds under the new interval.
    send_fixture_prompt(Path::new(cwd), INST_A, "after resume prompt", t105);
    write_rollout_prompt_at(
        home.path(),
        "F-after",
        cwd,
        &t107.format(&Rfc3339).unwrap(),
        "after resume prompt",
    );
    assert!(
        a_resumed.discover().unwrap(),
        "the new interval attributes a rollout created after resume"
    );
    assert_eq!(
        a_resumed.binding().map(|b| b.session_id.as_str()),
        Some("F-after"),
        "it binds the post-resume rollout, never the gap one"
    );
    drop(a_resumed);
    resumed.close_lifetime_at(t100 + time::Duration::seconds(30));
    drop(resumed);
}

#[test]
fn the_initial_prompt_survives_thirty_two_later_prompts_for_a_live_discoverer() {
    // r10 item 7 trigger 1 (the 32-entry cap): B is a live unbound discoverer.
    // Its first prompt P creates a lazily-written rollout F inside P's window,
    // but F is discovered only AFTER B has dispatched 33 further distinct
    // prompts. The old single Vec drained P (the oldest) once it held 32 later
    // entries, so F no longer had any matching evidence. The initial prompt is
    // now an anchor outside the capped recent list: B still binds F.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r10-item7-cap";
    let base = OffsetDateTime::now_utc() - time::Duration::seconds(60);

    let mut b = launch_adapter_keyed(home.path(), cwd, base, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, "initial prompt P", base);
    for i in 0..33u32 {
        send_fixture_prompt(
            Path::new(cwd),
            INST_B,
            &format!("later distinct prompt {i}"),
            base + time::Duration::seconds(10 + i64::from(i)),
        );
    }
    // F is written (and discovered) only now, created inside P's window.
    let f_at = base + time::Duration::seconds(3);
    write_rollout_prompt_at(
        home.path(),
        "F-anchor-cap",
        cwd,
        &f_at.format(&Rfc3339).unwrap(),
        "initial prompt P",
    );
    assert!(
        b.discover().unwrap(),
        "the anchored initial prompt still authorizes F after 33 later prompts"
    );
    assert_eq!(
        b.binding().map(|binding| binding.session_id.as_str()),
        Some("F-anchor-cap")
    );
}

#[test]
fn a_later_resend_keeps_the_initial_prompts_original_window() {
    // r10 item 7 trigger 2 (resend refreshes the window): B sends P, F is
    // created inside P's ORIGINAL window, then B RESENDS P several seconds
    // later. The old code merged the two into one entry and moved its time to
    // the resend, so the window's back edge (resend - 5s) slid past F and the
    // match was lost. The anchor keeps its original window; the resend opens a
    // separate later window — F still binds.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r10-item7-resend";
    let base = OffsetDateTime::now_utc() - time::Duration::seconds(60);

    let mut b = launch_adapter_keyed(home.path(), cwd, base, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, "continue", base);
    // F is created 2 s after the ORIGINAL send (inside [P-5s, P+300s]).
    let f_at = base + time::Duration::seconds(2);
    write_rollout_prompt_at(
        home.path(),
        "F-anchor-resend",
        cwd,
        &f_at.format(&Rfc3339).unwrap(),
        "continue",
    );
    // A resend 10 s on would, under the old merge, move the only evidence to
    // base+10 and push the back edge to base+5 > F.
    send_fixture_prompt(
        Path::new(cwd),
        INST_B,
        "continue",
        base + time::Duration::seconds(10),
    );
    assert!(
        b.discover().unwrap(),
        "F matches the anchor's ORIGINAL window despite the later resend"
    );
    assert_eq!(
        b.binding().map(|binding| binding.session_id.as_str()),
        Some("F-anchor-resend")
    );
}

#[test]
fn a_non_initial_prompt_also_survives_the_cap_for_a_live_discoverer() {
    // c-usagefu r11 item 10 trigger 1: the r10 anchor protected ONLY the first
    // prompt. B dispatches an ordinary prompt P as its SECOND send, then 33
    // more distinct prompts. P fell out of the 32-entry recent list, so a
    // rollout created inside P's window and discovered at the end could not
    // bind. Ruling: a live discovery keeps EVERY window until bind/close.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r11-item10-cap";
    let base = OffsetDateTime::now_utc() - time::Duration::seconds(60);

    let mut b = launch_adapter_keyed(home.path(), cwd, base, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, "warm-up prompt zero", base);
    // P is an ordinary (non-initial) dispatch.
    let p_at = base + time::Duration::seconds(2);
    send_fixture_prompt(Path::new(cwd), INST_B, "the middle prompt P", p_at);
    for i in 0..33u32 {
        send_fixture_prompt(
            Path::new(cwd),
            INST_B,
            &format!("later distinct prompt {i}"),
            base + time::Duration::seconds(10 + i64::from(i)),
        );
    }
    let f_at = p_at + time::Duration::seconds(2);
    write_rollout_prompt_at(
        home.path(),
        "F-middle-cap",
        cwd,
        &f_at.format(&Rfc3339).unwrap(),
        "the middle prompt P",
    );
    assert!(
        b.discover().unwrap(),
        "P's window is retained past 33 later dispatches, not drained by a cap"
    );
    assert_eq!(
        b.binding().map(|binding| binding.session_id.as_str()),
        Some("F-middle-cap")
    );
}

#[test]
fn a_resend_of_a_non_initial_prompt_keeps_its_earlier_window() {
    // c-usagefu r11 item 10 trigger 2: the r10 anchor trick protected only the
    // FIRST prompt from a resend merge. B sends one warm-up prompt, then Q; F
    // is created inside Q's ORIGINAL window; B RESENDS Q ten seconds later.
    // Under the old merge the single recent entry for Q moved to the resend,
    // sliding its window's back edge past F. Every dispatch is now its own
    // window, so Q's original evidence still binds F.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r11-item10-resend";
    let base = OffsetDateTime::now_utc() - time::Duration::seconds(60);

    let mut b = launch_adapter_keyed(home.path(), cwd, base, INST_B);
    send_fixture_prompt(Path::new(cwd), INST_B, "warm-up prompt zero", base);
    let q_at = base + time::Duration::seconds(2);
    send_fixture_prompt(Path::new(cwd), INST_B, "query Q", q_at);
    // F is created inside Q's original window.
    let f_at = q_at + time::Duration::seconds(2);
    write_rollout_prompt_at(
        home.path(),
        "F-q-resend",
        cwd,
        &f_at.format(&Rfc3339).unwrap(),
        "query Q",
    );
    // Resend 10 s on: under the old merge the only Q evidence moved to q_at+10
    // and its window's back edge (q_at+5) passed F (q_at+2).
    send_fixture_prompt(
        Path::new(cwd),
        INST_B,
        "query Q",
        q_at + time::Duration::seconds(10),
    );
    assert!(
        b.discover().unwrap(),
        "F matches Q's ORIGINAL window even for a non-initial resent prompt"
    );
    assert_eq!(
        b.binding().map(|binding| binding.session_id.as_str()),
        Some("F-q-resend")
    );
}

#[test]
fn a_unique_process_held_candidate_wins_over_a_weaker_prompt_candidate() {
    // r9 item 2: process-owned and prompt-only candidates must not be pooled
    // into one ambiguous set. B is the sole live instance and sent
    // "continue"; two same-cwd post-launch rollouts carry it:
    //   G — held open by B's own agent process (tier b);
    //   F — prompt-compatible only (a weaker foreign candidate).
    // Pooled, the two read as Ambiguous and B binds nothing; resolved by
    // tier, the unique process candidate G binds and F is ignored.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r9-item2-tiers";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let rfc = now.format(&Rfc3339).unwrap();

    write_rollout_prompt_at(home.path(), "F-weak", cwd, &rfc, "continue");
    write_rollout_prompt_at(home.path(), "G-held", cwd, &rfc, "continue");
    let dir = home.path().join("sessions/2026/09/14");
    let (probe, roots) = fake_ownership(9101);
    probe.hold(&dir.join("rollout-G-held.jsonl"), 9101);

    send_fixture_prompt(Path::new(cwd), INST_B, "continue", now);
    let mut b = launch_with_fake_ownership(home.path(), cwd, floor, INST_B, 9101, &probe, &roots);
    assert!(
        b.discover().unwrap(),
        "the unique process-held candidate G binds despite the weaker F"
    );
    assert_eq!(
        b.binding().map(|binding| binding.session_id.as_str()),
        Some("G-held")
    );
    assert_eq!(
        crate::adapters::codex_discovery::claimed_owner_of(&dir.join("rollout-F-weak.jsonl")),
        None,
        "F is never claimed"
    );
}

/// A prompt sent to a GROK pane must not authorize a codex bind with the
/// same first user message.
#[test]
fn a_grok_pane_prompt_does_not_authorize_a_codex_bind() {
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r6-grok";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);

    // A codex-like adapter, but the only recorded prompt in the cwd belongs
    // to a DIFFERENT (grok) instance key (as generic_pty records codex-only:
    // a grok pane records nothing on the codex registry at all).
    let mut codex = launch_adapter_keyed(home.path(), cwd, floor, "codex-key");
    write_rollout_prompt_at(
        home.path(),
        "grok-shaped",
        cwd,
        &now.format(&Rfc3339).unwrap(),
        "same words",
    );
    // Simulate "a prompt exists in the cwd under some other identity" — it
    // must not authorize this codex adapter.
    send_fixture_prompt(Path::new(cwd), "grok-key", "same words", now);
    assert!(!codex.discover().unwrap());
    assert!(codex.binding().is_none());
}

/// Two prompted panes racing for their own files each bind exactly theirs
/// (deterministic interleaving: both scan both files; claims decide).
#[test]
fn two_prompted_panes_each_bind_their_own_file() {
    // r8 item 1: two files each held open by its OWN process tree → each
    // adapter binds exactly its file, prompts identical or distinct no longer
    // matter.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r8-two";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let rfc = now.format(&Rfc3339).unwrap();

    write_rollout_prompt_at(home.path(), "A-file", cwd, &rfc, "A words");
    write_rollout_prompt_at(home.path(), "B-file", cwd, &rfc, "B words");
    let dir = home.path().join("sessions/2026/09/14");
    let (probe_a, roots_a) = fake_ownership(5001);
    let (probe_b, roots_b) = fake_ownership(5002);
    probe_a.hold(&dir.join("rollout-A-file.jsonl"), 5001);
    probe_b.hold(&dir.join("rollout-B-file.jsonl"), 5002);

    let mut a =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_A, 5001, &probe_a, &roots_a);
    let mut b =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_B, 5002, &probe_b, &roots_b);
    assert!(a.discover().unwrap());
    assert!(b.discover().unwrap());
    assert_eq!(a.binding().map(|x| x.session_id.as_str()), Some("A-file"));
    assert_eq!(b.binding().map(|x| x.session_id.as_str()), Some("B-file"));
}

// c-usagefu r6 item 2 / r9 item 8 --------------------------------------------

/// Ownership probe that rendezvouses two real discover() scans at one race
/// file: each scanner blocks in the gate until the test releases its pid, so
/// BOTH adapters have selected the same candidate before either claims.
struct RaceProbe {
    state: std::sync::Arc<RaceState>,
    pid: u32,
    race_file: PathBuf,
    /// Post-race ownership (the loser's own later file).
    extra: std::sync::Mutex<std::collections::HashMap<PathBuf, std::collections::HashSet<u32>>>,
}

struct RaceState {
    arrived: std::sync::Mutex<u32>,
    all_arrived: std::sync::Condvar,
    released: std::sync::Mutex<std::collections::HashSet<u32>>,
    gate_changed: std::sync::Condvar,
}

impl RaceProbe {
    fn hold_extra(&self, path: &Path, pid: u32) {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.extra
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_default()
            .insert(pid);
    }
}

impl crate::adapters::codex_ownership::RolloutOwnership for RaceProbe {
    fn file_held_in_tree(&self, path: &Path, root: u32) -> bool {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if key == self.race_file {
            // Rendezvous: signal arrival, then wait for the test to release
            // THIS scanner — this is the point both scans share, after
            // candidate selection and before claim_session.
            {
                let mut arrived = self
                    .state
                    .arrived
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *arrived += 1;
                self.state.all_arrived.notify_all();
            }
            let _released = self
                .state
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _released = self
                .state
                .gate_changed
                .wait_while(_released, |set| !set.contains(&self.pid))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return root == self.pid;
        }
        self.extra
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .is_some_and(|pids| pids.contains(&root))
    }
}

#[test]
fn two_real_scans_race_one_claim_and_the_loser_binds_its_own_file() {
    // r9 item 8: two adapters run their REAL discover() scans and BOTH select
    // the same unclaimed candidate W before either claims (a genuine claim
    // race, not a pre-claim gate rejection). The winner's claim commits
    // first; the loser's claim_session returns false, so it must keep NO
    // identity (confirmed/discovered/binding all empty), never take W even
    // after the winner closes, and then discover/bind its own file L.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r9-item8-race";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let rfc = now.format(&Rfc3339).unwrap();

    write_rollout_prompt_at(home.path(), "W", cwd, &rfc, "winner words");
    let dir = home.path().join("sessions/2026/09/14");
    let winner_path = dir.join("rollout-W.jsonl");
    let race_file = winner_path
        .canonicalize()
        .unwrap_or_else(|_| winner_path.to_path_buf());

    let state = std::sync::Arc::new(RaceState {
        arrived: std::sync::Mutex::new(0),
        all_arrived: std::sync::Condvar::new(),
        released: std::sync::Mutex::new(std::collections::HashSet::new()),
        gate_changed: std::sync::Condvar::new(),
    });
    let probe_a = std::sync::Arc::new(RaceProbe {
        state: std::sync::Arc::clone(&state),
        pid: 6001,
        race_file: race_file.clone(),
        extra: std::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let probe_b = std::sync::Arc::new(RaceProbe {
        state: std::sync::Arc::clone(&state),
        pid: 6002,
        race_file,
        extra: std::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let roots_a = std::sync::Arc::new(std::sync::Mutex::new(vec![6001u32]));
    let roots_b = std::sync::Arc::new(std::sync::Mutex::new(vec![6002u32]));

    // Both adapters exist and are unbound BEFORE any discover() call.
    let mut a = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: PathBuf::from(cwd),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_lifetime(crate::adapters::codex_discovery::begin_lifetime(
        Path::new(cwd),
        INST_A,
        Some(floor),
    ))
    .with_instance_key(Some(INST_A.into()))
    .with_ownership(
        probe_a.clone() as std::sync::Arc<dyn crate::adapters::codex_ownership::RolloutOwnership>,
        Some(std::sync::Arc::clone(&roots_a)),
    );
    let mut b = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: PathBuf::from(cwd),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_lifetime(crate::adapters::codex_discovery::begin_lifetime(
        Path::new(cwd),
        INST_B,
        Some(floor),
    ))
    .with_instance_key(Some(INST_B.into()))
    .with_ownership(
        probe_b.clone() as std::sync::Arc<dyn crate::adapters::codex_ownership::RolloutOwnership>,
        Some(std::sync::Arc::clone(&roots_b)),
    );
    assert!(a.confirmed.is_none() && a.discovered.is_none() && a.binding().is_none());
    assert!(b.confirmed.is_none() && b.discovered.is_none() && b.binding().is_none());

    // Both real scans run concurrently; the probes block them after they have
    // BOTH selected W. Release the WINNER first, then the loser: the loser's
    // claim_session races the already-committed winner claim and fails.
    std::thread::scope(|scope| {
        let winner = scope.spawn(|| a.discover().unwrap());
        let loser = scope.spawn(|| b.discover().unwrap());

        {
            let mut arrived = state
                .arrived
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while *arrived < 2 {
                arrived = state
                    .all_arrived
                    .wait(arrived)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        state
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(6001);
        state.gate_changed.notify_all();
        assert!(winner.join().expect("winner thread"), "winner claims W");

        state
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(6002);
        state.gate_changed.notify_all();
        assert!(
            !loser.join().expect("loser thread"),
            "the loser's claim fails and it stays unbound"
        );
    });

    // The loser carries no identity from the failed claim.
    assert!(b.confirmed.is_none(), "loser confirmed stays empty");
    assert!(b.discovered.is_none(), "loser discovered stays empty");
    assert!(b.binding().is_none(), "loser never bound W");
    assert_eq!(
        crate::adapters::codex_discovery::claimed_owner_of(&winner_path).as_deref(),
        Some(INST_A),
        "W is claimed by the winner alone"
    );
    assert_eq!(a.binding().map(|x| x.session_id.as_str()), Some("W"));

    // After the winner closes, the loser still cannot take W (the claim
    // outlives the close and the loser's process never held it).
    drop(a);
    assert!(!b.discover().unwrap());
    assert!(
        b.binding().is_none(),
        "closing the winner transfers nothing"
    );

    // The loser's own rollout appears and is held by its pid -> binds.
    write_rollout_prompt_at(home.path(), "L", cwd, &rfc, "loser words");
    let loser_path = dir.join("rollout-L.jsonl");
    probe_b.hold_extra(&loser_path, 6002);
    assert!(b.discover().unwrap());
    assert_eq!(
        b.binding().map(|x| x.session_id.as_str()),
        Some("L"),
        "the loser binds its own rollout, never W"
    );
    assert_eq!(
        crate::adapters::codex_discovery::claimed_owner_of(&winner_path).as_deref(),
        Some(INST_A),
        "W remains the winner's"
    );
}

// c-usagefu r6 item 3 ---------------------------------------------------------

#[test]
fn an_identical_prompt_sent_minutes_apart_refreshes_and_each_turn_binds() {
    // "continue" dispatched twice with a large gap must refresh the recorded
    // send time per instance (r5 dropped duplicate text globally). Both the
    // first session and the later one bind, even though their first prompt
    // text is byte-identical.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r6-refresh";

    // Turn 1 at t0.
    let t0 = OffsetDateTime::now_utc() - time::Duration::seconds(600);
    let floor = t0 - time::Duration::seconds(5);
    send_fixture_prompt(Path::new(cwd), INST_A, "continue", t0);
    let mut a1 = launch_adapter_keyed(home.path(), cwd, floor, INST_A);
    write_rollout_prompt_at(
        home.path(),
        "continue-1",
        cwd,
        &t0.format(&Rfc3339).unwrap(),
        "continue",
    );
    assert!(a1.discover().unwrap());
    drop(a1);

    // Turn 2, 600 s later (outside INPUT_MATCH_WINDOW of t0), same text.
    let t1 = OffsetDateTime::now_utc();
    send_fixture_prompt(Path::new(cwd), INST_A, "continue", t1);
    let mut a2 = launch_adapter_keyed(home.path(), cwd, t1 - time::Duration::seconds(5), INST_A);
    write_rollout_prompt_at(
        home.path(),
        "continue-2",
        cwd,
        &t1.format(&Rfc3339).unwrap(),
        "continue",
    );
    assert!(
        a2.discover().unwrap(),
        "the refreshed send time lets the second identical prompt bind"
    );
    assert_eq!(
        a2.binding().map(|b| b.session_id.as_str()),
        Some("continue-2")
    );
}

// c-usagefu r6 item 7 ---------------------------------------------------------

#[test]
fn promoted_adapter_drop_releases_claim_after_unlink_with_exact_key() {
    // r7 item 6: the promoted adapter's Drop must release using the exact
    // PathBuf recorded at bind time, driven by dropping AFTER the file and
    // symlink are gone. A release that re-canonicalizes the stored
    // main_file path post-unlink cannot find the canonical key and leaks.
    use std::os::unix::fs::symlink;

    let real_home = tempfile::tempdir().unwrap();
    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("codex-link");
    symlink(real_home.path(), &link).unwrap();
    let session_dir = link.join("sessions/2026/09/14");
    std::fs::create_dir_all(&session_dir).unwrap();
    let rollout = session_dir.join("rollout-drop-sym.jsonl");
    std::fs::write(
        &rollout,
        "{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"id\":\"drop-s\",\"session_id\":\"drop-s\"}}\n",
    )
    .unwrap();
    std::fs::write(link.join("session_index.jsonl"), "{\"id\":\"drop-s\"}\n").unwrap();

    // Promoted adapter (no instance key): binds via the name index.
    let home_path: std::path::PathBuf = link.clone();
    let adapter = CodexAdapter::new(AdapterHome {
        home: home_path.clone(),
        cwd: home_path.clone(),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    });
    let mut adapter = adapter;
    assert!(adapter.discover().unwrap());
    let exact_key = adapter.claim_key.clone().expect("bind stored a claim key");
    assert!(
        crate::adapters::codex_discovery::is_claim_present_exact(
            &exact_key,
            &adapter.lifecycle_owner
        ),
        "claim is live before unlink"
    );

    // Remove the rollout file AND the symlink BEFORE dropping the adapter.
    std::fs::remove_file(&rollout).unwrap();
    std::fs::remove_file(&link).unwrap();
    drop(adapter);

    assert!(
        !crate::adapters::codex_discovery::is_claim_present_exact(
            &exact_key,
            // owner is random per adapter; re-query isn't possible post-drop,
            // so check the key is absent for ANY owner by trying to reclaim.
            "anyone"
        ),
        "sanity placeholder"
    );
    // Stronger check: the exact key maps to no owner at all after Drop.
    assert!(
        !crate::adapters::codex_discovery::claim_is_present_any_owner(&exact_key),
        "post-unlink Drop released the exact claim key (no ghost)"
    );
}

#[test]
fn claim_release_uses_the_exact_key_not_a_re_canonicalized_path() {
    // r6 item 7: a promoted adapter releases with the exact key recorded at
    // claim time. A release call using any OTHER path string — e.g. the
    // original symlink path after the claim key was canonicalized, or a path
    // that no longer resolves the same way after a move — must not touch the
    // stored entry. Also covers symlinked CODEX_HOME and file deletion: the
    // release happens after the file is gone and still clears the entry.
    use std::os::unix::fs::symlink;

    let real_home = tempfile::tempdir().unwrap();
    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("codex-link");
    symlink(real_home.path(), &link).unwrap();
    let rollout_dir = link.join("sessions/2026/09/14");
    std::fs::create_dir_all(&rollout_dir).unwrap();
    let rollout = rollout_dir.join("rollout-sym.jsonl");
    std::fs::write(&rollout, "header\n").unwrap();

    let owner = "promoted-owner";
    assert!(crate::adapters::codex_discovery::claim_session(
        &rollout, owner, None
    ));
    let key = crate::adapters::codex_discovery::claim_key_for(&rollout);
    assert_ne!(key, rollout, "the claim key is the canonical real path");
    assert!(
        crate::adapters::codex_discovery::is_claimed_by_other(&rollout, "someone-else"),
        "visible through the symlink before release"
    );

    // A release with the symlink path (the caller's original path) must not
    // remove the canonical-key entry — this is the ghost the review calls out
    // (the wrong key must never match, whether or not the FS still resolves).
    crate::adapters::codex_discovery::release_claim_key(&rollout, owner);
    assert!(
        crate::adapters::codex_discovery::is_claim_present_exact(&key, owner),
        "a non-canonical key must not release the claim"
    );

    // Delete the file and the symlink, then release with the RECORDED key:
    // it clears without touching the filesystem.
    std::fs::remove_file(&rollout).unwrap();
    std::fs::remove_file(&link).unwrap();
    crate::adapters::codex_discovery::release_claim_key(&key, owner);
    assert!(
        !crate::adapters::codex_discovery::is_claim_present_exact(&key, owner),
        "exact recorded key releases after the path and symlink are gone"
    );
}

// c-usagefu r7 item 1: EXCLUSIVE attribution -------------------------------

/// r8 item 1: two live instances send identical "continue". Prompt text is
/// never attribution once the epoch is shared; B binds ONLY because its
/// process holds the rollout open (tier b). A never binds it; closing A
/// changes nothing (sticky shared epoch does not hand the file to B).
#[test]
fn identical_prompts_two_live_owners_never_cross_bind() {
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r8-exclusive";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let rfc = now.format(&Rfc3339).unwrap();

    send_fixture_prompt(Path::new(cwd), INST_A, "continue", now);
    send_fixture_prompt(Path::new(cwd), INST_B, "continue", now);
    write_rollout_prompt_at(home.path(), "B-only", cwd, &rfc, "continue");
    let b_path = home
        .path()
        .join("sessions/2026/09/14")
        .join("rollout-B-only.jsonl");
    let (probe_a, roots_a) = fake_ownership(7001);
    let (probe_b, roots_b) = fake_ownership(7002);

    let mut a =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_A, 7001, &probe_a, &roots_a);
    let mut b =
        launch_with_fake_ownership(home.path(), cwd, floor, INST_B, 7002, &probe_b, &roots_b);

    // A scans first: A's process holds no fd on B's file → unbound.
    assert!(!a.discover().unwrap(), "A never binds B's process file");
    assert!(a.binding().is_none());
    // B's process holds it → binds even with A live and identical prompts.
    probe_b.hold(&b_path, 7002);
    assert!(
        b.discover().unwrap(),
        "B binds via process ownership, not text"
    );
    assert_eq!(b.binding().map(|x| x.session_id.as_str()), Some("B-only"));

    // A closes: still cannot bind — shared epoch + no process ownership.
    drop(a);
    // A is already dropped; construct a fresh C in the same (now unshared but
    // previously-shared) epoch to prove sticky text never cross-binds.
    let (probe_c, roots_c) = fake_ownership(7003);
    send_fixture_prompt(Path::new(cwd), "inst-c-key", "continue", now);
    let mut c = launch_with_fake_ownership(
        home.path(),
        cwd,
        floor,
        "inst-c-key",
        7003,
        &probe_c,
        &roots_c,
    );
    assert!(
        !c.discover().unwrap(),
        "new owner with identical text never takes B's file"
    );
}

/// Reverse order: B scans before A exists at all (A's window opens after B's
/// file is present). A must still not bind once A appears.
#[test]
fn identical_prompts_late_opening_window_does_not_cross_bind() {
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r7-exclusive2";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);
    let rfc = now.format(&Rfc3339).unwrap();

    send_fixture_prompt(Path::new(cwd), INST_B, "continue", now);
    write_rollout_prompt_at(home.path(), "B-file", cwd, &rfc, "continue");
    let mut b = launch_adapter_keyed(home.path(), cwd, floor, INST_B);
    // B alone: exclusive → binds.
    assert!(b.discover().unwrap());
    assert_eq!(b.binding().map(|b| b.session_id.as_str()), Some("B-file"));

    // A launches later and sends the same text; B's file is already claimed
    // by B (excluded), so A never binds it.
    send_fixture_prompt(Path::new(cwd), INST_A, "continue", now);
    let mut a = launch_adapter_keyed(home.path(), cwd, floor, INST_A);
    assert!(!a.discover().unwrap());
    assert!(a.binding().is_none());
}

// c-usagefu r7 item 2: promoted adapters have unique lifecycle owners ----

#[test]
fn two_promoted_adapters_cannot_co_claim_the_same_indexed_session() {
    // Both adapters have no launch floor (promoted/hand-run); the newest
    // indexed session resolves to the same file. Only one must bind.
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path();
    let session_dir = home.path().join("sessions/2026/09/14");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("rollout-shared.jsonl"),
        "{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"id\":\"shared-s\",\"session_id\":\"shared-s\"}}\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("session_index.jsonl"),
        "{\"id\":\"shared-s\"}\n",
    )
    .unwrap();

    let mut first = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    });
    let mut second = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    });
    assert!(first.discover().unwrap(), "first promoted adapter binds");
    assert!(
        !second.discover().unwrap(),
        "second promoted adapter must not co-claim the same file"
    );
    assert!(second.binding().is_none());
}

// c-usagefu r7 item 3: stable instance-id claims survive close/resume -----

#[test]
fn stable_instance_key_reclaims_after_close_and_resume() {
    // Simulates the native-carrier shell-pty lifecycle: the first spawn
    // claims a rollout under the stable Remuda instance id, its adapter
    // closes on close/resume, and a fresh adapter with the SAME key reclaims.
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path();
    let session_dir = home.path().join("sessions/2026/09/14");
    std::fs::create_dir_all(&session_dir).unwrap();
    let rollout = session_dir.join("rollout-resume.jsonl");
    std::fs::write(
        &rollout,
        "{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"id\":\"resume-s\",\"session_id\":\"resume-s\"}}\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("session_index.jsonl"),
        "{\"id\":\"resume-s\"}\n",
    )
    .unwrap();

    let stable_key = "ins-stable-resume";
    {
        // First spawn: native-carrier adapter with stable key (no launch
        // floor? shell-pty driver launches DO pass instance_key with floor
        // None here for promoted-style attach — but the stable key is present).
        let mut first = CodexAdapter::new(AdapterHome {
            home: home.path().to_path_buf(),
            cwd: cwd.to_path_buf(),
            pid: None,
            launched_at: None,
            resume_session_id: None,
        })
        .with_instance_key(Some(stable_key.to_owned()));
        assert!(first.discover().unwrap());
        assert_eq!(
            first.binding().map(|b| b.session_id.as_str()),
            Some("resume-s")
        );
        // First spawn closes — stable driver-launch claims are NOT released.
    }

    // A DIFFERENT instance key cannot grab the file after close.
    let mut foreign = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    })
    .with_instance_key(Some("ins-other".to_owned()));
    assert!(!foreign.discover().unwrap());

    // Resume: fresh adapter, same stable key → reclaims.
    let mut resumed = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    })
    .with_instance_key(Some(stable_key.to_owned()));
    assert!(
        resumed.discover().unwrap(),
        "resumed adapter with the stable instance key reclaims the rollout"
    );
    assert_eq!(
        resumed.binding().map(|b| b.session_id.as_str()),
        Some("resume-s")
    );
}

// c-usagefu r7 item 10: age + lifecycle eviction of sent inputs ----------

#[test]
fn a_prompt_older_than_max_age_is_evicted_and_cannot_authorize() {
    use time::Duration;
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r7-aged";
    let now = OffsetDateTime::now_utc();

    // Record a prompt 2 hours ago (older than INPUT_MAX_AGE=1h): the act of
    // recording a newer prompt triggers eviction of the old one.
    send_fixture_prompt(
        Path::new(cwd),
        INST_A,
        "old prompt",
        now - Duration::hours(2),
    );
    send_fixture_prompt(Path::new(cwd), INST_A, "fresh prompt", now);

    // A rollout matching the OLD text at the old time must not bind.
    let mut adapter = launch_adapter_keyed(
        home.path(),
        cwd,
        now - Duration::hours(2) - Duration::seconds(5),
        INST_A,
    );
    write_rollout_prompt_at(
        home.path(),
        "old-file",
        cwd,
        &(now - Duration::hours(2)).format(&Rfc3339).unwrap(),
        "old prompt",
    );
    assert!(
        !adapter.discover().unwrap(),
        "aged-out prompt cannot authorize a rollout"
    );
    drop(adapter);
}

#[test]
fn closing_the_last_discovery_window_clears_recorded_prompts() {
    // After all discovery windows in a cwd close (every instance bound or
    // gave up), the recorded prompts are discarded: a later fresh adapter
    // with no sent prompt must not match a rollout left behind.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r7-lifecycle";
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(5);

    let mut a = launch_adapter_keyed(home.path(), cwd, floor, INST_A);
    send_fixture_prompt(Path::new(cwd), INST_A, "bound words", now);
    write_rollout_prompt_at(
        home.path(),
        "bound",
        cwd,
        &now.format(&Rfc3339).unwrap(),
        "bound words",
    );
    assert!(a.discover().unwrap()); // binds → releases the window (last one)

    // A brand new adapter after the lifecycle ended has no prompts recorded.
    let mut late = launch_adapter_keyed(home.path(), cwd, floor, "late-key");
    assert!(
        !late.discover().unwrap(),
        "prompts from the closed lifecycle are gone"
    );
    assert!(late.binding().is_none());
}

// c-usagefu r8 item 7 / r9 item 6: age sweep preserves a live discoverer's
// prompts -----------------------------------------------------------------

#[test]
fn age_eviction_keeps_the_only_prompt_of_a_live_unbound_discoverer() {
    use time::Duration;
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r8-item7-livekeep";
    let now = OffsetDateTime::now_utc();

    // A launches and stays live, still unbound. Its only prompt was sent 90
    // minutes ago; the rollout (created within the prompt's 5-minute match
    // window) is discovered only now.
    let mut a = launch_adapter_keyed(home.path(), cwd, now - Duration::minutes(91), INST_A);
    let prompt_at = now - Duration::minutes(90);
    send_fixture_prompt(Path::new(cwd), INST_A, "late discovered prompt", prompt_at);

    // An UNRELATED instance records a prompt at `now`: the global age sweep
    // fired by that write must not strip the live discoverer's only evidence.
    send_fixture_prompt(
        Path::new("/projects/r8-item7-other-cwd"),
        "other-key",
        "unrelated",
        now,
    );

    write_rollout_prompt_at(
        home.path(),
        "late-file",
        cwd,
        &(prompt_at + Duration::seconds(10))
            .format(&Rfc3339)
            .unwrap(),
        "late discovered prompt",
    );
    assert!(
        a.discover().unwrap(),
        "live discoverer binds its rollout from aged-but-retained evidence"
    );
    assert_eq!(
        a.binding().map(|b| b.session_id.as_str()),
        Some("late-file")
    );
}

#[test]
fn a_fresh_prompt_does_not_evict_a_live_discoverers_aged_first_prompt() {
    // r9 item 6: live unbound A sent initial prompt P 90 minutes ago; the
    // rollout for P was created inside P's match window but is discovered
    // only now. A sends a FRESH Q in the meantime. The r8 rule retained aged
    // prompts only when ALL of A's prompts were aged, so the Q-triggered
    // sweep deleted P and P's file could never bind.
    use time::Duration;
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r9-item6-pq";
    let now = OffsetDateTime::now_utc();

    let mut a = launch_adapter_keyed(home.path(), cwd, now - Duration::minutes(91), INST_A);
    let p_at = now - Duration::minutes(90);
    send_fixture_prompt(Path::new(cwd), INST_A, "the initial prompt P", p_at);

    // A sends its FRESH Q ...
    send_fixture_prompt(Path::new(cwd), INST_A, "fresh follow-up Q", now);
    // ... and a LATER input (another cwd, as any record_input does) fires the
    // sweep that now sees both the aged P and the fresh Q. Under r8 this
    // deleted P ("not ALL prompts aged"); r9 must keep P.
    send_fixture_prompt(
        Path::new("/projects/r9-item6-other"),
        "other-key",
        "unrelated later input",
        now,
    );

    // P's rollout is discovered only now (created inside P's match window).
    write_rollout_prompt_at(
        home.path(),
        "P-file",
        cwd,
        &(p_at + Duration::seconds(10)).format(&Rfc3339).unwrap(),
        "the initial prompt P",
    );
    assert!(
        a.discover().unwrap(),
        "P survives the Q-triggered sweep and binds its rollout"
    );
    assert_eq!(a.binding().map(|b| b.session_id.as_str()), Some("P-file"));
}

#[test]
fn once_bound_the_aged_prompt_is_reclaimed_by_the_lifecycle() {
    // r9 item 6 counterpart: the retention is bounded by the lifecycle. A
    // live adapter keeps its aged prompt only while UNBOUND; binding
    // releases the window, and a subsequent sweep in another cwd removes it.
    use time::Duration;
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r9-item6-reclaim";
    let now = OffsetDateTime::now_utc();

    let mut a = launch_adapter_keyed(home.path(), cwd, now - Duration::minutes(91), INST_A);
    let p_at = now - Duration::minutes(90);
    send_fixture_prompt(Path::new(cwd), INST_A, "old prompt", p_at);
    write_rollout_prompt_at(
        home.path(),
        "P-bound",
        cwd,
        &(p_at + Duration::seconds(10)).format(&Rfc3339).unwrap(),
        "old prompt",
    );
    assert!(a.discover().unwrap(), "binds from retained aged evidence");
    assert!(a.window.is_none(), "bind released the window");

    // A fresh unrelated cwd sweep now evicts A's aged prompt (no live
    // discoverer retains it); a later adapter in the cwd has no prompts.
    send_fixture_prompt(
        Path::new("/projects/r9-item6-other2"),
        "other-key",
        "unrelated",
        now,
    );
    let mut late = launch_adapter_keyed(home.path(), cwd, now, "late-key");
    assert!(
        !late.discover().unwrap(),
        "after bind the aged prompt is reclaimed, not retained forever"
    );
    assert!(late.binding().is_none());
}

// c-usagefu r8 item 5: aggregator-rejected usage is not emitted -----------

#[test]
fn a_duplicate_usage_record_emits_only_one_message_observation() {
    // Two identical token_usage_record lines (same response id): the
    // UsageAggregator accepts the first and rejects the second, so only one
    // Message observation is produced.
    let line = |ordinal: u32| {
        format!(
            "{{\"ordinal\":{ordinal},\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"tdup\",\"response_id\":\"rdup\",\"usage\":{{\"input_tokens\":0,\"cached_input_tokens\":0,\"cache_creation_input_tokens\":0,\"output_tokens\":10000,\"reasoning_output_tokens\":0,\"total_tokens\":10000}}}}}}\n"
        )
    };
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: tempfile::tempdir().unwrap().path().to_path_buf(),
        cwd: PathBuf::from("/w"),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    });
    let first = adapter.on_line(1, "dup-fixture", 0, &line(1));
    let second = adapter.on_line(2, "dup-fixture", 120, &line(2));
    let msg_count = |v: &[AdapterObservation]| {
        v.iter()
            .filter(|o| matches!(&o.payload, ObservationPayload::Usage(u) if u.scope == UsageScope::Message))
            .count()
    };
    assert_eq!(msg_count(&first), 1, "first record emits one Message");
    assert_eq!(
        msg_count(&second),
        0,
        "duplicate rejected by aggregator → no Message"
    );
}

#[test]
fn a_usage_record_without_response_id_emits_a_durable_message_identity() {
    // c-usagefu r11 item 7 (Ruling B2), tightened by r12 item 3 (B2'): a
    // per-response token_usage_record that reports NO response_id is still a
    // counted call, emitted as a Message under the durable scope id = (rollout
    // file identity, byte offset of the record) — the SAME identity every
    // counted record now carries, never a per-poll counter.
    let line = |ordinal: u32, turn: &str| {
        format!(
            "{{\"ordinal\":{ordinal},\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"{turn}\",\"usage\":{{\"input_tokens\":0,\"cached_input_tokens\":0,\"cache_creation_input_tokens\":0,\"output_tokens\":20,\"reasoning_output_tokens\":0,\"total_tokens\":20}}}}}}\n"
        )
    };
    let mut adapter = CodexAdapter::new(AdapterHome {
        home: tempfile::tempdir().unwrap().path().to_path_buf(),
        cwd: PathBuf::from("/w"),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    });

    let obs = adapter.on_line(7, "rollout-F", 412, &line(7, "t1"));
    let message = obs
        .iter()
        .find_map(|o| match &o.payload {
            ObservationPayload::Usage(u) if u.scope == UsageScope::Message => Some(u),
            _ => None,
        })
        .expect("the no-response-id record still emits a Message");
    assert_eq!(
        message.scope_id,
        super::durable_response_scope_id("rollout-F", 412),
        "the durable fallback scope id is file identity + byte offset"
    );

    // A different offset on the same file is a distinct response identity.
    let other = adapter.on_line(8, "rollout-F", 540, &line(8, "t2"));
    let other = other
        .iter()
        .find_map(|o| match &o.payload {
            ObservationPayload::Usage(u) if u.scope == UsageScope::Message => Some(u),
            _ => None,
        })
        .expect("a second no-id record emits a Message too");
    assert_ne!(other.scope_id, message.scope_id);

    // Re-tailing the same byte emits the SAME fallback id, so the Hub dedupes
    // the replay.
    let replayed = adapter.on_line(7, "rollout-F", 412, &line(7, "t1"));
    let replayed = replayed
        .iter()
        .find_map(|o| match &o.payload {
            ObservationPayload::Usage(u) if u.scope == UsageScope::Message => Some(u),
            _ => None,
        })
        .expect("replay re-emits through on_line (Hub dedupes by scope id)");
    assert_eq!(replayed.scope_id, message.scope_id, "stable across replay");
}

#[test]
fn a_byte_zero_replay_of_a_real_rollout_keeps_durable_response_identities() {
    // c-usagefu r11 item 7 (Ruling B2; r12 item 3 makes it the identity of
    // EVERY record) production-path regression: two no-id token_usage_record
    // lines arrive in SEPARATE tail polls of a real rollout file. A fresh
    // adapter then re-tails the SAME file from byte zero (a restart/replay).
    // The durable Message scope ids must be identical in both passes — the identity is (file identity, byte offset), so neither
    // poll boundaries nor a fresh tail change it. The old per-poll counter
    // restarted at 1 on every poll and on every adapter, emitting different
    // ids on replay, so the Hub could not dedupe and double counted.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-byteoffset.jsonl");
    let record = |ordinal: u32, turn: &str, output: u32| {
        format!(
            "{{\"ordinal\":{ordinal},\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"{turn}\",\"usage\":{{\"input_tokens\":0,\"cached_input_tokens\":0,\"cache_creation_input_tokens\":0,\"output_tokens\":{output},\"reasoning_output_tokens\":0,\"total_tokens\":{output}}}}}}}\n"
        )
    };
    let header = "{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"id\":\"s-bo\",\"session_id\":\"s-bo\"}}\n";
    // Blank lines deliberately vary byte offsets away from line numbers.
    let batch_one = format!("{header}\n{}", record(1, "t1", 20));
    let batch_two = format!("\n{}\n", record(3, "t2", 30));

    // A restart re-spawns the adapter with the SAME stable instance key (r7
    // item 3), which is what lets both passes re-claim the same rollout; a
    // launch floor makes the bind byte-zero.
    let bind = |home: &Path| {
        let mut adapter = CodexAdapter::new(AdapterHome {
            home: home.to_path_buf(),
            cwd: home.to_path_buf(),
            pid: None,
            launched_at: Some(OffsetDateTime::now_utc()),
            resume_session_id: None,
        })
        .with_instance_key(Some("replay-instance".to_owned()));
        adapter.bind_rollout("s-bo", path.clone());
        adapter
    };
    let message_ids = |v: Vec<AdapterObservation>| {
        v.into_iter()
            .filter_map(|o| match o.payload {
                ObservationPayload::Usage(u) if u.scope == UsageScope::Message => Some(u.scope_id),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    // Byte offsets of the two records in the file (computed before the bytes
    // are moved into fs::write).
    let offset_of_t1 = batch_one.len() - record(1, "t1", 20).len();
    let offset_of_t2 = batch_one.len() + "\n".len();

    // Incremental tailing: records land in two separate polls.
    std::fs::write(&path, batch_one).unwrap();
    let mut live = bind(dir.path());
    let first = message_ids(live.poll().unwrap());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(batch_two.as_bytes())
        .unwrap();
    let second = message_ids(live.poll().unwrap());
    let incremental_ids = {
        let mut ids = first.clone();
        ids.extend(second);
        ids
    };
    assert_eq!(
        incremental_ids.len(),
        2,
        "both no-id records emitted Messages"
    );

    // Byte-0 replay through a fresh adapter: identical identities in order.
    let mut replay = bind(dir.path());
    let replayed_ids = message_ids(replay.poll().unwrap());
    assert_eq!(
        replayed_ids, incremental_ids,
        "byte-0 replay reproduces the exact (file, offset) fallback ids"
    );
    // And the offsets are genuinely the file positions, not poll ordinals.
    assert!(
        replayed_ids[0].contains(&format!(":v2:{offset_of_t1}:")),
        "t1 id {replayed_ids:?} carries byte offset {offset_of_t1}"
    );
    assert!(
        replayed_ids[1].contains(&format!(":v2:{offset_of_t2}:")),
        "t2 id {replayed_ids:?} carries byte offset {offset_of_t2}"
    );
}

/// Write a rollout with an explicit session_meta timestamp/cwd (c-usagefu
/// r15 item 3 resume-by-id tests).
fn r15_write_rollout(home: &Path, id: &str, cwd: &str, at: &str) -> PathBuf {
    let session_dir = home.join("sessions/2026/10/01");
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join(format!("rollout-{id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{{\"timestamp\":\"{at}\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"session_id\":\"{id}\",\"cwd\":\"{cwd}\",\"timestamp\":\"{at}\"}}}}\n"
        ),
    )
    .unwrap();
    path
}

#[test]
fn a_resumed_launch_binds_the_named_pre_floor_rollout_and_refuses_every_other_file() {
    // c-usagefu r15 item 3: a real `codex resume <id>` appends to the EXISTING
    // rollout, whose session_meta.timestamp is the original start — before the
    // resume floor. The resumed adapter binds that file BY ID (cwd verified),
    // not by the timestamp scan, and never binds another pre-floor file in the
    // same cwd.
    use time::Duration;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let home = tempfile::tempdir().unwrap();
    let original_start = OffsetDateTime::now_utc() - Duration::seconds(3600);
    r15_write_rollout(
        home.path(),
        "r15-resume-session",
        "/r15item3",
        &original_start.format(&Rfc3339).unwrap(),
    );
    // A DIFFERENT pre-floor session, same cwd — indistinguishable to the
    // timestamp floor (both are before it); only the id may decide.
    r15_write_rollout(
        home.path(),
        "r15-other-session",
        "/r15item3",
        &(original_start + Duration::seconds(10))
            .format(&Rfc3339)
            .unwrap(),
    );

    let mut adapter = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: "/r15item3".into(),
        pid: None,
        launched_at: Some(OffsetDateTime::now_utc()),
        resume_session_id: Some("r15-resume-session".to_owned()),
    })
    .with_instance_key(Some("r15-item3-owner".to_owned()));

    assert!(
        adapter.discover().unwrap(),
        "the resumed session binds by id even though its header predates the floor"
    );
    assert_eq!(
        adapter.binding().unwrap().session_id,
        "r15-resume-session",
        "the other pre-floor file is refused"
    );
}

#[test]
fn a_named_resume_rollout_in_another_cwd_fails_closed_without_guessing() {
    // c-usagefu r15 item 3: the located rollout's cwd must still match; a
    // name-index/homedir collision must never bind a foreign-cwd session under
    // the resume instruction, and discovery never falls through to the
    // same-cwd file or the name-index guess.
    use time::Duration;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let home = tempfile::tempdir().unwrap();
    let original_start = OffsetDateTime::now_utc() - Duration::seconds(3600);
    // The named session lives in a DIFFERENT cwd.
    r15_write_rollout(
        home.path(),
        "r15-foreign-session",
        "/somewhere-else",
        &original_start.format(&Rfc3339).unwrap(),
    );
    // A same-cwd session the floor scan would otherwise be tempted by.
    r15_write_rollout(
        home.path(),
        "r15-samecwd-session",
        "/r15item3",
        &original_start.format(&Rfc3339).unwrap(),
    );

    let mut adapter = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: "/r15item3".into(),
        pid: None,
        launched_at: Some(OffsetDateTime::now_utc()),
        resume_session_id: Some("r15-foreign-session".to_owned()),
    });

    assert!(
        !adapter.discover().unwrap(),
        "a foreign-cwd named session is refused, with no index/floor fallback"
    );
    assert!(adapter.binding().is_none(), "nothing else is bound");
}

#[test]
fn a_confirmed_launch_samples_its_boundary_inside_the_registration_lock() {
    // c-usagefu r15 item 4 (Ruling L1) on the hook-confirmed shell-pty path:
    // register_confirmed_launch samples the boundary INSIDE
    // LAUNCH_REGISTRATION. A registration parked before the lock must carry
    // the post-release instant, never the instant it decided to launch.
    use crate::adapters::codex_discovery::PendingLaunchBarrierHandle as PendingLaunchBarrier;
    use std::time::Duration as StdDuration;
    use time::OffsetDateTime;

    let cwd = "/projects/r15-item4-confirmed-boundary";
    let owner = "r15-item4-confirmed";
    let barrier = PendingLaunchBarrier::arm_pre_lock(Path::new(cwd));
    let (tx, rx) = std::sync::mpsc::channel();
    let builder = std::thread::Builder::new()
        .name("r15-item4-confirmed-launch".into())
        .spawn(move || {
            let registration = crate::adapters::codex_discovery::register_confirmed_launch(
                Path::new(cwd),
                owner,
                None,
            );
            tx.send(registration).unwrap();
        })
        .unwrap();
    barrier.wait_before_lock();
    // World moves for over a second while the registration is queued.
    let before = OffsetDateTime::now_utc();
    std::thread::sleep(StdDuration::from_millis(1100));
    barrier.release_pre_lock();
    let (guard, boundary) = rx.recv().unwrap();
    builder.join().unwrap();
    assert!(
        boundary - before >= time::Duration::milliseconds(900),
        "the confirmed-launch boundary is sampled inside the lock, after the wait"
    );
    drop(guard);
    crate::adapters::codex_discovery::forget_lifetime(Path::new(cwd), owner);
}

#[test]
fn with_instance_key_opens_no_window_when_a_lifetime_is_already_attached() {
    // c-usagefu r15 item 4: the shell-pty spawn attaches its pre-registered
    // lifetime witness. with_instance_key must then NOT open a
    // DiscoveryWindow (hook-confirmed launches do no prompt-only discovery);
    // the old shape opened one anyway.
    use time::OffsetDateTime;

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let floor = OffsetDateTime::now_utc() - time::Duration::seconds(2);
    let owner = "r15-item4-window";

    // The witness the production shell-pty spawn attaches.
    let guard = crate::adapters::codex_discovery::begin_lifetime(&cwd, owner, Some(floor));
    let adapter = CodexAdapter::new(AdapterHome {
        home: dir.path().join("shadow"),
        cwd: cwd.clone(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_lifetime(guard.witness())
    .with_instance_key(Some(owner.to_owned()));
    assert_eq!(
        crate::adapters::codex_discovery::open_window_count(&cwd),
        0,
        "an attached lifetime suppresses the discovery window"
    );
    drop(adapter);
    assert_eq!(
        crate::adapters::codex_discovery::open_window_count(&cwd),
        0,
        "no window was opened or leaked"
    );
    drop(guard);
    crate::adapters::codex_discovery::forget_lifetime(&cwd, owner);

    // Control: with NO pre-registered lifetime the window DOES open (the
    // unbundled direct-construction discovery path).
    let control = CodexAdapter::new(AdapterHome {
        home: dir.path().join("shadow2"),
        cwd: cwd.clone(),
        pid: None,
        launched_at: Some(floor),
        resume_session_id: None,
    })
    .with_instance_key(Some("r15-item4-window-control".to_owned()));
    assert_eq!(
        crate::adapters::codex_discovery::open_window_count(&cwd),
        1,
        "without a lifetime the legacy window-open behaviour is unchanged"
    );
    drop(control);
}

#[test]
fn prompt_evidence_is_bounded_by_text_length_and_window_count_even_while_live() {
    // c-usagefu r15 item 4: SENT_INPUTS must never accumulate prompt text
    // without bound — each stored prompt is capped and the per-owner window
    // count is capped oldest-first, live discoverer included. A genuine long
    // prompt still matches because the rollout text is bounded identically.
    use crate::adapters::codex_discovery::DiscoveryWindow;
    use time::OffsetDateTime;

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let owner = "r15-item4-prompts";
    let now = OffsetDateTime::now_utc();
    // A LIVE unbound discoverer: its window used to exempt prompts from every
    // cap.
    let window = DiscoveryWindow::open(&cwd, owner);

    // One document-sized dispatch is stored only up to the text cap.
    let long_prompt = "x".repeat(20_000);
    crate::adapters::codex_discovery::record_input_at(&cwd, owner, &long_prompt, now);
    let texts = crate::adapters::codex_discovery::recorded_prompt_texts(&cwd, owner);
    assert_eq!(texts.len(), 1);
    assert_eq!(
        texts[0].chars().count(),
        8 * 1024,
        "the retained prompt text is capped at 8 KiB chars"
    );

    // 300 further dispatches: the window count stays bounded, oldest first.
    for i in 1..=300 {
        crate::adapters::codex_discovery::record_input_at(
            &cwd,
            owner,
            &format!("dispatch number {i}"),
            now + time::Duration::seconds(i as i64),
        );
    }
    assert_eq!(
        crate::adapters::codex_discovery::recorded_prompt_window_count(&cwd, owner),
        256,
        "prompt windows never accumulate past the per-owner cap"
    );
    let texts = crate::adapters::codex_discovery::recorded_prompt_texts(&cwd, owner);
    assert_eq!(
        texts[0], "dispatch number 45",
        "oldest windows were evicted first"
    );
    assert_eq!(
        texts[255], "dispatch number 300",
        "newest windows are retained"
    );
    drop(window);

    // The genuine long prompt still matches a rollout carrying it: both
    // normalize to the same bounded prefix.
    let owner2 = "r15-item4-longmatch";
    let floor = now + time::Duration::minutes(2);
    let _lifetime = crate::adapters::codex_discovery::begin_lifetime(&cwd, owner2, Some(floor));
    crate::adapters::codex_discovery::record_input_at(&cwd, owner2, &long_prompt, floor);
    let started = floor + time::Duration::seconds(10);
    assert!(
        crate::adapters::codex_discovery::sole_instance_matches(
            &cwd,
            owner2,
            &long_prompt,
            started
        ),
        "a real over-cap prompt still matches its rollout through the shared bounded prefix"
    );
    crate::adapters::codex_discovery::forget_lifetime(&cwd, owner2);
}

#[test]
fn prompts_sharing_a_long_prefix_but_differing_in_suffix_never_bind() {
    // c-usagefu r16 item 6: the match key is a collision-resistant digest of
    // the COMPLETE trimmed prompt plus its length — NOT the bounded display
    // prefix. Two distinct prompts that are byte-identical for the first 8 KiB
    // but differ afterwards must never author each other's rollout.
    use crate::adapters::codex_discovery as disc;
    use time::OffsetDateTime;

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let owner = "r16-item6-prefix";
    let now = OffsetDateTime::now_utc();
    let floor = now + time::Duration::minutes(2);
    let _lifetime = disc::begin_lifetime(&cwd, owner, Some(floor));

    let shared = "z".repeat(8 * 1024);
    let sent = format!("{shared}suffix-one");
    let other = format!("{shared}suffix-two");
    assert_ne!(sent, other);
    // The exact collision shape the old bounded-prefix compare could not see:
    // the retained/rollout prefixes are identical through the whole 8 KiB.
    assert_eq!(sent[..shared.len()], other[..shared.len()]);

    disc::record_input_at(&cwd, owner, &sent, floor);
    let started = floor + time::Duration::seconds(10);

    // The genuine, COMPLETE prompt still binds (digest over the whole text).
    assert!(
        disc::sole_instance_matches(&cwd, owner, &sent, started),
        "the complete dispatched prompt matches its rollout"
    );
    // Whitespace around the full prompt does not change the fingerprint.
    assert!(
        disc::sole_instance_matches(&cwd, owner, &format!("  {sent}\n"), started),
        "trimming is applied before the digest"
    );
    // The shared-prefix / different-suffix rollout must NOT bind.
    assert!(
        !disc::sole_instance_matches(&cwd, owner, &other, started),
        "identical prefix, different suffix never bind"
    );

    // Same refusal through the PRODUCTION discovery entry point (the call the
    // real adapter poll makes), with a distinct rollout path for each.
    let wrong_rollout = dir.path().join("rollout-r16item6-wrong.jsonl");
    let right_rollout = dir.path().join("rollout-r16item6-right.jsonl");
    std::fs::write(&wrong_rollout, b"x").unwrap();
    std::fs::write(&right_rollout, b"x").unwrap();
    assert!(
        !disc::prompt_candidate_matches(&cwd, owner, &wrong_rollout, &other, started),
        "production discovery refuses the shared-prefix rollout"
    );
    assert!(
        disc::prompt_candidate_matches(&cwd, owner, &right_rollout, &sent, started),
        "production discovery binds the exact full-prompt rollout"
    );

    disc::forget_lifetime(&cwd, owner);
}

#[test]
fn dropping_the_barrier_handle_disarms_the_parked_registration_and_clears_the_slot() {
    // c-usagefu r15 item 8(c): the handle returned by arm owns the barrier.
    // Dropping it (even while a parked registration still holds an Arc clone)
    // must (1) release the parked registration and (2) clear the cwd slot via
    // the Arc::ptr_eq check, so the NEXT registration proceeds without any
    // explicit release. An older dropped handle must never clear a NEWER
    // barrier's slot.
    use crate::adapters::codex_discovery::PendingLaunchBarrierHandle as Barrier;
    use crate::adapters::codex_discovery::{forget_lifetime, register_pending_launch};

    let cwd = "/projects/r15-item8-barrier-drop";

    // (1) Drop releases the parked registration.
    let barrier = Barrier::arm_pre_lock(Path::new(cwd));
    let (tx, rx) = std::sync::mpsc::channel();
    let builder = std::thread::Builder::new()
        .name("r15-item8-parked".into())
        .spawn(move || {
            tx.send(register_pending_launch(Path::new(cwd), "r15-item8-b", None))
                .unwrap();
        })
        .unwrap();
    barrier.wait_before_lock();
    drop(barrier);
    let pending = rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("dropping the handle releases the parked registration");
    builder.join().unwrap();

    // (2) The slot was cleared: a fresh registration completes with NO
    // barrier armed and NO release call.
    let (tx2, rx2) = std::sync::mpsc::channel();
    let builder2 = std::thread::Builder::new()
        .name("r15-item8-clear".into())
        .spawn(move || {
            tx2.send(register_pending_launch(Path::new(cwd), "r15-item8-c", None))
                .unwrap();
        })
        .unwrap();
    rx2.recv_timeout(std::time::Duration::from_secs(3))
        .expect("the slot is clear: nothing parks a later registration");
    builder2.join().unwrap();
    forget_lifetime(Path::new(cwd), "r15-item8-b");
    forget_lifetime(Path::new(cwd), "r15-item8-c");
    drop(pending);

    // (3) A dropped OLDER handle never removes the NEWER barrier's slot.
    let older = Barrier::arm_pre_lock(Path::new(cwd));
    let newer = Barrier::arm_pre_lock(Path::new(cwd));
    drop(older);
    let (tx3, rx3) = std::sync::mpsc::channel();
    let builder3 = std::thread::Builder::new()
        .name("r15-item8-newer-park".into())
        .spawn(move || {
            tx3.send(register_pending_launch(Path::new(cwd), "r15-item8-d", None))
                .unwrap();
        })
        .unwrap();
    newer.wait_before_lock();
    assert!(
        rx3.recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "the newer barrier still parks after the older handle dropped"
    );
    drop(newer);
    rx3.recv_timeout(std::time::Duration::from_secs(3))
        .expect("dropping the newer handle releases the registration");
    builder3.join().unwrap();
    forget_lifetime(Path::new(cwd), "r15-item8-d");
}

#[test]
fn dropping_the_barrier_handle_clears_the_slot_even_while_the_lock_is_contended() {
    // c-usagefu r16 item 10: the slot removal must take the slot lock AFTER
    // disarming with a BLOCKING lock. The old try_lock skipped removal while
    // another thread transiently held the map mutex, leaking the stale barrier
    // into the next launch. Hold the map from a second thread, run the handle's
    // Drop concurrently, then release — the slot must be absent afterwards.
    use crate::adapters::codex_discovery::PendingLaunchBarrierHandle as Barrier;
    use crate::adapters::codex_discovery::{
        lock_pending_launch_slot_for_test, pending_launch_barrier_slot_absent,
    };

    let cwd = "/projects/r16-item10-slot-contention";
    let barrier = Barrier::arm_pre_lock(Path::new(cwd));
    assert!(
        !pending_launch_barrier_slot_absent(Path::new(cwd)),
        "arming installs the barrier slot"
    );

    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::Builder::new()
        .name("r16-item10-slot-holder".into())
        .spawn(move || {
            let _guard = lock_pending_launch_slot_for_test();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
        .unwrap();
    locked_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("the holder acquired the slot lock");

    // Drop runs on its own thread: it disarms first, then BLOCKS on the slot
    // lock until the holder releases.
    let dropper = std::thread::Builder::new()
        .name("r16-item10-dropper".into())
        .spawn(move || drop(barrier))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        !dropper.is_finished(),
        "the dropping handle is waiting for the contended slot lock"
    );

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    dropper
        .join()
        .expect("the drop completes once the slot lock is free");

    assert!(
        pending_launch_barrier_slot_absent(Path::new(cwd)),
        "the slot is actually removed despite the lock being contended during Drop"
    );
}
