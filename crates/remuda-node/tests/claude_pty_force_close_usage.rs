//! c-ctxusage r13 item 1: when the native writer does NOT finish on Ctrl-C
//! within the stop-phase grace (or its status RPCs fail during Herdr's
//! shutdown window), close must not run the final transcript drain while the
//! writer is still alive — its last usage append would land after the pump
//! retired and be lost. The stop phase instead force-terminates the writer
//! (closes its agent pane, reaping the foreground process group) and VERIFIES
//! its exit, still keeping the transcript collector and the observation
//! sender alive; only then does the drain run.
//!
//! The fake-herdr delayed-writer model leaves a live agent in the agent list
//! on ctrl+c (it outlives the fault-shortened grace deterministically — no
//! timing inference) and appends the final record only when the agent pane is
//! force-closed, modelling the writer flushing on the forced SIGHUP/SIGTERM.
//! Before the fix the force happened in reap, AFTER the drain, so the record
//! was lost.

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

const FORCED_MSG_ID: &str = "msg_forced_usage";

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000008".parse().unwrap(),
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
        "uuid": format!("cp-fc-a-{index}"),
        "timestamp": "2026-10-09T00:00:00.000Z",
        "isSidechain": false,
        "message": {
            "id": format!("cp-fc-msg-{index}"),
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

/// The record the delayed writer flushes when its pane is force-closed after
/// the Ctrl-C grace expired. Only a force-before-drain close can read it.
fn forced_record() -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": "cp-forced-record",
        "timestamp": "2026-10-09T00:01:00.000Z",
        "isSidechain": false,
        "message": {
            "id": FORCED_MSG_ID,
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": null,
            "content": [{"type": "text", "text": "FORCED"}],
            "usage": {
                "input_tokens": 909,
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
        instance_id: "ins_cp_force".into(),
        seq: observation.seq.0 as i64,
        event_id: format!("evt_cp_fc_{}", observation.seq.0),
        observed_at: String::from(observation.observed_at.clone()),
        event: serde_json::to_value(observation).expect("serialize observation"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_force_terminates_a_writer_outliving_the_grace_before_the_drain() {
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
    // The delayed writer's final record lives OUTSIDE the transcript until
    // the agent pane is force-closed.
    let forced_record_file = launch.join("forced-record.jsonl");
    fs::write(&forced_record_file, forced_record()).unwrap();

    // Delayed-writer model: ctrl+c leaves the agent live past the grace; the
    // record is flushed only on the pane's forced close.
    let _fake = FakeHerdrServer::spawn(
        FakeHerdrOptions::new(&socket).force_close_appends_record(&transcript, &forced_record_file),
    )
    .unwrap();

    // Shorten the stop grace so grace expiry -> forced termination is fast and
    // deterministic. The fake never removes the agent on ctrl+c, so the polite
    // exit path CANNOT succeed in this test — only the verified force can.
    let faults = TranscriptFaults::new();
    faults.arm_agent_exit_grace(Duration::from_millis(300));

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-force".into(),
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

    let forced_count = Arc::new(AtomicU32::new(0));
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let (forced_tx, forced_rx) = tokio::sync::oneshot::channel();
    let forced_tx = std::sync::Mutex::new(Some(forced_tx));
    let drainer_count = Arc::clone(&forced_count);
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
                && payload.scope_id == FORCED_MSG_ID
            {
                drainer_count.fetch_add(1, AtomicOrdering::SeqCst);
                if let Some(tx) = forced_tx.lock().unwrap().take() {
                    let _ = tx.send(obs);
                }
            }
        }
    });

    // Acknowledgement barrier: the seeded replay bound the transcript; the
    // delayed writer is live and nothing force-related has happened yet.
    tokio::time::timeout(Duration::from_secs(30), bound_rx)
        .await
        .expect("transcript never bound (timed out)")
        .expect("bound oneshot closed without firing");
    assert_eq!(forced_count.load(AtomicOrdering::SeqCst), 0);

    // Close: ctrl+c cannot move the fake agent, the shortened grace expires,
    // the driver force-closes the agent pane, verifies the exit, and only
    // then drains — the forced record reaches the observation stream.
    driver.close().await.expect("close");

    let rescued = tokio::time::timeout(Duration::from_secs(60), forced_rx)
        .await
        .expect("the force-terminated writer's final usage never reached the stream")
        .expect("forced oneshot closed without a frame");
    // Drain to receiver closure: close is bounded, so a leaked sender fails
    // the test instead of hanging; and a duplicate FINAL cannot hide in the
    // channel after the first delivery.
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed after close")
        .expect("event drainer panicked");
    assert_eq!(
        forced_count.load(AtomicOrdering::SeqCst),
        1,
        "the force-terminated writer's usage must be delivered exactly once"
    );

    // The rescued usage survives the real Hub pipeline.
    let conn = rusqlite::Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");
    let record = to_journal_record(&rescued);
    let row = project_usage_event(&record, None, None).expect("projects usage");
    assert_eq!(row.scope, "turn");
    assert_eq!(row.scope_id.as_deref(), Some(FORCED_MSG_ID));
    assert!(insert_usage_event(&conn, &row).expect("insert usage row"));
    assert_eq!(row.input_tokens, Some(909));
    assert_eq!(row.output_tokens, Some(9));
    assert_eq!(row.cache_read_tokens, Some(1_010));
    assert_eq!(row.cache_write_tokens, Some(7));
}
