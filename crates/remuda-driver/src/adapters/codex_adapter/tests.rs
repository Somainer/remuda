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
        launched_at: None,
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
    });
    let observed = feed(&mut adapter, lines);
    let usages: Vec<_> = observed
        .iter()
        .filter(|o| matches!(o.payload, ObservationPayload::Usage(_)))
        .collect();
    // c-usagefu (b): one per-response Message snapshot, then the end-of-turn
    // Turn snapshot and the cumulative Session snapshot.
    assert_eq!(usages.len(), 3);
    let message = match &usages[0].payload {
        ObservationPayload::Usage(usage) => usage,
        _ => unreachable!(),
    };
    let turn = match &usages[1].payload {
        ObservationPayload::Usage(usage) => usage,
        _ => unreachable!(),
    };
    let session = match &usages[2].payload {
        ObservationPayload::Usage(usage) => usage,
        _ => unreachable!(),
    };
    assert_eq!(message.scope, remuda_protocol::UsageScope::Message);
    assert_eq!(turn.scope, remuda_protocol::UsageScope::Turn);
    assert_eq!(session.scope, remuda_protocol::UsageScope::Session);
    // The Message row is keyed on the native response id.
    assert_eq!(message.scope_id, "r1");
    // Turn and Session rows carry the per-turn / cumulative counters; the
    // per-request Message row is the context basket source.
    assert_eq!(turn.scope_id, "t1");
    // c-ctxusage r5 item 7: the Turn/Session rows carry the rollout record's
    // own timestamp, so a byte-0 replay is historical evidence, not
    // ingest-time throughput. The task_complete record stamps ...:45.380Z.
    for observation in &usages[1..] {
        let value = observation
            .native_at
            .as_ref()
            .expect("usage carries the rollout record timestamp");
        assert_eq!(
            String::from(value.clone()),
            "2026-09-13T18:21:45.380Z",
            "native_at is the task_complete record time"
        );
    }
    // Both estimated; cumulative token_count did not double the tokens.
    for payload in [turn, session] {
        assert_eq!(payload.accounting, remuda_protocol::Accounting::Estimated);
    }
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
    });

    assert!(adapter.poll().unwrap().is_empty());
    let bound = adapter.binding().map(|binding| binding.session_id.clone());
    assert!(
        bound.is_none(),
        "no foreign session may bind, got {bound:?}"
    );

    // The pane's own session registers shortly after launch; the driver
    // dispatched its prompt just before that.
    send_fixture_prompt(cwd, "fixture driver prompt", now);
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

/// Two same-cwd sessions inside the launch window is unprovable ownership:
/// fail closed permanently, even after one of them goes away.
#[test]
fn two_post_launch_same_cwd_rollouts_fail_closed_for_the_launch_lifetime() {
    let home = tempfile::tempdir().unwrap();
    let cwd = Path::new("/projects/alpha-r3amb");
    // Keep the discovery window open so this exercises the Ambiguous branch,
    // not the deadline branch.
    let now = OffsetDateTime::now_utc();
    let floor = now - time::Duration::seconds(10);
    let rfc = |at: OffsetDateTime| at.format(&Rfc3339).unwrap();
    send_fixture_prompt(cwd, "fixture driver prompt", now);
    write_rollout(
        home.path(),
        "2026/09/14",
        "one",
        "/projects/alpha-r3amb",
        &rfc(now - time::Duration::seconds(5)),
    );
    write_rollout(
        home.path(),
        "2026/09/14",
        "two",
        "/projects/alpha-r3amb",
        &rfc(now - time::Duration::seconds(1)),
    );

    let mut adapter = CodexAdapter::new(AdapterHome {
        home: home.path().to_path_buf(),
        cwd: cwd.to_path_buf(),
        pid: None,
        launched_at: Some(floor),
    });
    assert!(!adapter.discover().unwrap(), "ambiguous binds nothing");
    assert!(adapter.binding().is_none());

    // Remove one candidate so a unique match would now exist; the ambiguity
    // decision stands and the home is not rescanned forever.
    std::fs::remove_file(
        home.path()
            .join("sessions/2026/09/14")
            .join("rollout-two.jsonl"),
    )
    .unwrap();
    assert!(!adapter.discover().unwrap());
    assert!(adapter.binding().is_none());
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
    });
    assert!(!adapter.discover().unwrap());
    assert!(adapter.binding().is_none());
    assert!(
        !adapter.discovery_gave_up,
        "NotYet never gives up; the poll loop keeps discovering until bound"
    );

    // First prompt at t+200 s: the driver dispatches it and the rollout's
    // session starts just now.
    let now = OffsetDateTime::now_utc();
    send_fixture_prompt(cwd, "fixture driver prompt", now);
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

/// Record a driver-sent prompt (as generic_pty.send does) for unit tests.
/// `cwd` must be the same path given to the adapter's `AdapterHome`.
fn send_fixture_prompt(cwd: &Path, prompt: &str, at: OffsetDateTime) {
    crate::adapters::codex_discovery::record_input_at(cwd, prompt, at);
}

/// Build an unbound driver-launch adapter (with an open discovery window).
fn launch_adapter(home: &Path, cwd: &str, floor: OffsetDateTime) -> CodexAdapter {
    CodexAdapter::new(AdapterHome {
        home: home.to_path_buf(),
        cwd: Path::new(cwd).to_path_buf(),
        pid: None,
        launched_at: Some(floor),
    })
}

#[test]
fn a_bound_first_instance_releases_its_window_and_a_later_instance_binds_its_own() {
    // r5 item 1: A binds at 10:01 and its window must not linger; B launched
    // in the same cwd later is never tainted, and A's claimed rollout is
    // invisible to B, so B binds B's own file.
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
        "fixture driver prompt",
        now - time::Duration::seconds(50),
    );
    let mut a = launch_adapter(home.path(), cwd, floor_a);
    write_rollout(home.path(), "2026/09/14", "A-thread", cwd, &a_started);
    assert!(a.discover().unwrap(), "A binds its unique rollout");
    assert!(
        a.window.is_none(),
        "binding releases the discovery window immediately"
    );

    // B launches later with a fresh floor after A's session started.
    let floor_b = now - time::Duration::seconds(10);
    let mut b = launch_adapter(home.path(), cwd, floor_b);
    assert!(
        !b.window.as_ref().is_some_and(|window| window.overlapping()),
        "A is no longer discovering: B opens clean"
    );
    assert!(
        !b.discover().unwrap(),
        "A's claimed rollout must not bind to B"
    );
    assert!(b.binding().is_none());

    // B's own lazily-created rollout appears after B's prompt; unique and
    // unclaimed -> binds.
    send_fixture_prompt(
        Path::new(cwd),
        "fixture driver prompt",
        now - time::Duration::seconds(2),
    );
    let later = (now - time::Duration::seconds(2)).format(&Rfc3339).unwrap();
    write_rollout(home.path(), "2026/09/14", "B-thread", cwd, &later);
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

    let mut a = launch_adapter(home.path(), cwd, floor);
    let mut b = launch_adapter(home.path(), cwd, floor);
    let mut c = launch_adapter(home.path(), cwd, floor);
    for adapter in [&a, &b, &c] {
        assert!(
            adapter
                .window
                .as_ref()
                .is_some_and(|window| window.overlapping()),
            "three seekers overlap"
        );
    }

    // A's rollout appears and A wins it; B and C must not latch give-up and
    // must not touch A's file.
    send_fixture_prompt(Path::new(cwd), "fixture driver prompt", now);
    write_rollout(
        home.path(),
        "2026/09/14",
        "A1",
        cwd,
        &now.format(&Rfc3339).unwrap(),
    );
    // Exactly one poll across the three claims A1 (others see zero unclaimed
    // or a lost claim); drive a couple of ticks so the claim resolves.
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
    assert_eq!(bound.as_deref(), Some("A1"), "exactly one adapter binds A1");
    for adapter in [&b, &c] {
        assert!(
            !adapter.discovery_gave_up,
            "overlap never latches a permanent give-up"
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
fn an_idle_unbound_instance_does_not_block_a_later_second_instance() {
    // A launched but never prompted (idle, window still open). B launches in
    // the same cwd; while both overlap B waits, but when A is closed B binds
    // its own rollout — A's lingering window cannot poison B for its life.
    let home = tempfile::tempdir().unwrap();
    let cwd = "/projects/r5-idle";
    let floor = OffsetDateTime::now_utc() - time::Duration::seconds(5);
    let mut a = launch_adapter(home.path(), cwd, floor);
    assert!(!a.discover().unwrap(), "A has no rollout yet");

    let mut b = launch_adapter(home.path(), cwd, floor);
    assert!(
        b.window.as_ref().is_some_and(|window| window.overlapping()),
        "B overlaps the still-idle A"
    );

    // A is closed (instance torn down without ever being prompted).
    drop(a);

    let now = OffsetDateTime::now_utc();
    // B dispatched its own prompt before the rollout was created.
    send_fixture_prompt(Path::new(cwd), "fixture driver prompt", now);
    write_rollout(
        home.path(),
        "2026/09/14",
        "B-only",
        cwd,
        &now.format(&Rfc3339).unwrap(),
    );
    assert!(b.discover().unwrap());
    assert_eq!(
        b.binding().map(|binding| binding.session_id.as_str()),
        Some("B-only")
    );
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
    send_fixture_prompt(Path::new(cwd), "fixture driver prompt", now);
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
    // the strict floor) could be a crash-looped previous instance: tail from
    // EOF so its records never replay, even though it is an acceptable
    // candidate.
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
    send_fixture_prompt(Path::new(cwd), "fixture driver prompt", now);
    assert!(
        adapter.discover().unwrap(),
        "the in-slack file still matches"
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

    let mut adapter = launch_adapter(home.path(), cwd, floor);
    // The driver sent the prompt slightly before the rollout was created.
    record_input_at(&cwd_dir, prompt, now);
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
