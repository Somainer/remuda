//! c-ctxusage r12 item 1: on `ClaudePtyDriver::close`, the driver must stop
//! the native writer and acknowledge its termination WHILE the transcript
//! collector and the observation sender are still alive, and only then drain
//! the final transcript append. Claude appends the interrupted turn's usage
//! record WHILE HANDLING Ctrl-C; with the old order (drain, then
//! `resources.close()` which sent Ctrl-C) that append landed after the pump
//! retired and was lost.
//!
//! The fake-herdr interrupt model appends a no-stop usage record to the
//! transcript the moment a live agent pane receives ctrl+c and then removes
//! the agent. The regression asserts the interrupted turn's usage reaches the
//! observation stream AND survives the real Hub projection/insertion.

#![cfg(unix)]

use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, DriverKind, LaunchOrigin,
    ProviderHealth, ProviderKind, ProviderProfile, pin_binary,
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

const INTERRUPTED_MSG_ID: &str = "msg_interrupted_usage";

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000007".parse().unwrap(),
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
        "uuid": format!("cp-int-a-{index}"),
        "timestamp": "2026-10-09T00:00:00.000Z",
        "isSidechain": false,
        "message": {
            "id": format!("cp-int-msg-{index}"),
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

/// The record the native writer appends WHILE HANDLING Ctrl-C: the
/// interrupted turn, no stop_reason, real counters. Only the post-interrupt
/// final drain can read it.
fn interrupted_record() -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": "cp-interrupted-record",
        "timestamp": "2026-10-09T00:01:00.000Z",
        "isSidechain": false,
        "message": {
            "id": INTERRUPTED_MSG_ID,
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": null,
            "content": [{"type": "text", "text": "INTERRUPTED"}],
            "usage": {
                "input_tokens": 808,
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
        instance_id: "ins_cp_interrupt".into(),
        seq: observation.seq.0 as i64,
        event_id: format!("evt_cp_int_{}", observation.seq.0),
        observed_at: String::from(observation.observed_at.clone()),
        event: serde_json::to_value(observation).expect("serialize observation"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_interrupts_the_native_before_the_drain_so_ctrl_c_usage_is_recovered() {
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
    // The record the fake appends on ctrl+c lives OUTSIDE the transcript until
    // the interrupt lands.
    let interrupt_record_file = launch.join("interrupted-record.jsonl");
    fs::write(&interrupt_record_file, interrupted_record()).unwrap();

    let _fake = FakeHerdrServer::spawn(
        FakeHerdrOptions::new(&socket)
            .interrupt_appends_record(&transcript, &interrupt_record_file),
    )
    .unwrap();

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-interrupt".into(),
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

    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let (interrupted_tx, interrupted_rx) = tokio::sync::oneshot::channel();
    let interrupted_tx = std::sync::Mutex::new(Some(interrupted_tx));
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
                && payload.scope_id == INTERRUPTED_MSG_ID
                && let Some(tx) = interrupted_tx.lock().unwrap().take()
            {
                let _ = tx.send(obs);
            }
        }
    });

    // Acknowledgement barrier: the seeded replay bound the transcript.
    tokio::time::timeout(Duration::from_secs(30), bound_rx)
        .await
        .expect("transcript never bound (timed out)")
        .expect("bound oneshot closed without firing");

    // Close: interrupt the native (the fake appends the interrupted-turn
    // record on ctrl+c and reports the agent gone), then drain.
    driver.close().await.expect("close");

    let rescued = tokio::time::timeout(Duration::from_secs(60), interrupted_rx)
        .await
        .expect("the interrupted turn usage never reached the observation stream")
        .expect("interrupted oneshot closed without a frame");
    // Drain to channel close: the close is bounded, so a sender leak fails
    // here instead of hanging.
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed after close")
        .expect("event drainer panicked");

    // The rescued usage survives the real Hub pipeline.
    let conn = rusqlite::Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");
    let record = to_journal_record(&rescued);
    let row = project_usage_event(&record, None, None).expect("projects usage");
    assert_eq!(row.scope, "turn");
    assert_eq!(row.scope_id.as_deref(), Some(INTERRUPTED_MSG_ID));
    assert!(insert_usage_event(&conn, &row).expect("insert usage row"));
    assert_eq!(row.input_tokens, Some(808));
    assert_eq!(row.output_tokens, Some(9));
    assert_eq!(row.cache_read_tokens, Some(1_010));
    assert_eq!(row.cache_write_tokens, Some(7));
}
