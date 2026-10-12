//! c-ctxusage r13 item 2: `ClaudePtyDriver::close` phases must be resumable.
//!
//! close marks `live.closed` BEFORE the native ownership probe. When that
//! probe fails on the first attempt (Herdr refusing `session.snapshot` in its
//! shutdown window) the call returns the error, but a retried close used to
//! take the `live.closed` short circuit — skipping the verified stop, the
//! final transcript drain, the task joins and the live retirement — so the
//! interrupted turn's usage never arrived and resources were never reaped
//! through the close-time sequence.
//!
//! The injected fault fails EXACTLY the first close's stop-phase probe (the
//! native writer is never signalled on that attempt, proven by the transcript
//! file still lacking the interrupt record). The second close resumes: sends
//! Ctrl-C, the fake appends the interrupted-turn record and terminates, the
//! final drain reads it, and the receiver runs to channel closure.

#![cfg(unix)]

use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, DriverError, DriverKind,
    LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile, pin_binary,
};
use remuda_hub::usage_store_test_support::{
    JournalRecord, insert_usage_event, migrate, project_usage_event,
};
use remuda_protocol::{InstanceSpec, ObservationPayload};
use remuda_testing::{
    FakeHerdrOptions, FakeHerdrServer, ShortTempDir, ensure_workspace_bin, install_executable,
};
use std::fs;
use std::path::Path;
use std::time::Duration;

const RESUMED_MSG_ID: &str = "msg_resumed_usage";

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000009".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn spec(cwd: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec = serde_json::from_str(include_str!(
        "../../remuda-driver/tests/fixtures/instance-spec.json"
    ))
    .unwrap();
    spec.driver = DriverKind::ClaudePty;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("haiku".into());
    spec
}

fn filler(index: u32) -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": format!("cp-rs-a-{index}"),
        "timestamp": "2026-10-09T00:00:00.000Z",
        "isSidechain": false,
        "message": {
            "id": format!("cp-rs-msg-{index}"),
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "x"}],
            "usage": {
                "input_tokens": 11,
                "cache_read_input_tokens": 22,
                "cache_creation_input_tokens": 1,
                "output_tokens": 3
            }
        }
    });
    format!("{record}\n")
}

/// The record the writer appends WHILE HANDLING the second close's Ctrl-C.
fn interrupted_record() -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": "cp-resumed-record",
        "timestamp": "2026-10-09T00:01:00.000Z",
        "isSidechain": false,
        "message": {
            "id": RESUMED_MSG_ID,
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": null,
            "content": [{"type": "text", "text": "RESUMED"}],
            "usage": {
                "input_tokens": 707,
                "cache_read_input_tokens": 1_010,
                "cache_creation_input_tokens": 7,
                "output_tokens": 9
            }
        }
    });
    format!("{record}\n")
}

fn to_journal_record(observation: &remuda_protocol::Observation) -> JournalRecord {
    JournalRecord {
        instance_id: "ins_cp_resume".into(),
        seq: observation.seq.0 as i64,
        event_id: format!("evt_cp_rs_{}", observation.seq.0),
        observed_at: String::from(observation.observed_at.clone()),
        event: serde_json::to_value(observation).expect("serialize observation"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_failing_the_ownership_probe_is_completed_by_a_retry() {
    use remuda_driver::test_faults::TranscriptFaults;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_root = ShortTempDir::new().unwrap();
    let socket_dir = socket_root.as_ref().join("herdr");
    fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join("herdr.sock");
    let fake_bin = ensure_workspace_bin("fake-herdr");

    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let claude = install_executable(
        tmp.path(),
        "claude",
        "#!/bin/sh\necho '2.1.268 (Claude Code)'\n",
    );

    fs::create_dir_all(&launch).unwrap();
    let transcript = launch.join("transcript.jsonl");
    {
        use std::io::Write;
        let mut file = fs::File::create(&transcript).expect("create transcript");
        for index in 0..10u32 {
            file.write_all(filler(index).as_bytes()).unwrap();
        }
        file.sync_all().unwrap();
    }
    let interrupt_record_file = launch.join("interrupted-record.jsonl");
    fs::write(&interrupt_record_file, interrupted_record()).unwrap();

    let _fake = FakeHerdrServer::spawn(
        FakeHerdrOptions::new(&socket)
            .interrupt_appends_record(&transcript, &interrupt_record_file),
    )
    .unwrap();

    // Exactly ONE close attempt fails the stop-phase ownership probe.
    let faults = TranscriptFaults::new();
    faults.arm_close_probe_failures(1);

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-resume".into(),
        socket_dir: Some(socket_dir),
        herdr_binary: Some(fake_bin),
        broker: std::sync::Arc::new(remuda_driver::EnvFileSecretBroker::env_only()),
        extra_env: Default::default(),
        agent_mcp: None,
        setting_sources: None,
        agent_start_timeout_ms: 5_000,
        inherit_default_config: false,
        settings_overlay_path: None,
        auto_trust_registered_workspace: false,
        seed_onboarding: true,
        host_claude_config: None,
    });
    driver.set_test_faults(faults.clone());

    let handle = driver.start(spec(&cwd)).await.expect("start");
    let mut events_rx = handle.into_events();
    fs::write(
        launch.join("session-meta.json"),
        serde_json::json!({
            "session_id": "fixture-session",
            "transcript_path": transcript,
            "hook_event_name": "SessionStart",
        })
        .to_string(),
    )
    .unwrap();

    let resumed_count = Arc::new(AtomicU32::new(0));
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let (resumed_tx, resumed_rx) = tokio::sync::oneshot::channel();
    let resumed_tx = std::sync::Mutex::new(Some(resumed_tx));
    let drainer_count = Arc::clone(&resumed_count);
    let drainer = tokio::spawn(async move {
        while let Some(obs) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(payload) = &obs.body
                && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &obs.body
                && payload.scope_id == RESUMED_MSG_ID
            {
                drainer_count.fetch_add(1, AtomicOrdering::SeqCst);
                if let Some(tx) = resumed_tx.lock().unwrap().take() {
                    let _ = tx.send(obs);
                }
            }
        }
    });

    tokio::time::timeout(Duration::from_secs(30), bound_rx)
        .await
        .expect("transcript never bound (timed out)")
        .expect("bound oneshot closed without firing");

    // First close: the ownership probe fails, exactly as Herdr's shutdown
    // window would refuse session.snapshot. The native writer is never
    // signalled.
    let first = driver.close().await;
    assert!(
        matches!(first, Err(DriverError::ControlUnavailable)),
        "the failed probe must surface as a close error, got {first:?}"
    );

    // Positive proof the stop phase never ran on the failed attempt: the
    // interrupt record still has not been appended to the transcript file.
    let transcript_after_failure = fs::read_to_string(&transcript).expect("read transcript");
    assert!(
        !transcript_after_failure.contains(RESUMED_MSG_ID),
        "the failed close must not have interrupted the native writer"
    );

    // Second close resumes every pending phase: stop the writer, drain the
    // final append, reap and retire.
    driver.close().await.expect("the retried close must finish");

    let rescued = tokio::time::timeout(Duration::from_secs(60), resumed_rx)
        .await
        .expect("the retried close never delivered the interrupted-turn usage")
        .expect("resumed oneshot closed without a frame");
    // Receiver closure: the retried close retires every sender; a leaked
    // sender (or a duplicate FINAL buffered past the first delivery) fails
    // this bounded join instead of hiding.
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed after the retried close")
        .expect("event drainer panicked");
    assert_eq!(
        resumed_count.load(AtomicOrdering::SeqCst),
        1,
        "the interrupted-turn usage must be delivered exactly once across both closes"
    );

    // The rescued usage survives the real Hub pipeline.
    let conn = rusqlite::Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");
    let record = to_journal_record(&rescued);
    let row = project_usage_event(&record, None, None).expect("projects usage");
    assert_eq!(row.scope, "turn");
    assert_eq!(row.scope_id.as_deref(), Some(RESUMED_MSG_ID));
    assert!(insert_usage_event(&conn, &row).expect("insert usage row"));
    assert_eq!(row.input_tokens, Some(707));
    assert_eq!(row.output_tokens, Some(9));
    assert_eq!(row.cache_read_tokens, Some(1_010));
    assert_eq!(row.cache_write_tokens, Some(7));
}
