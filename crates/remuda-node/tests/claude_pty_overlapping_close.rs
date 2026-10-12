//! c-ctxusage r14 item 5: two overlapping closes must emit the terminal
//! lifecycle exactly once.
//!
//! A purge can close a still-live instance while the queued
//! `instance.close` is in flight (node runtime.rs). Without a close-phase
//! owner both close bodies ran interleaved: the second one re-ran the reap
//! against a still-present workspace and emitted its OWN `carrier_closed`,
//! so consumers saw two terminal lifecycles for one instance.
//!
//! The test-only tail gate parks the FIRST close deterministically after
//! its verified stop and final drain — while still holding the close-phase
//! owner — and the second close is spawned only after a barrier proves it
//! has reached the close entry. The gate is then opened and both closes run
//! to completion; the observation channel closes and exactly one
//! carrier_closed is ever delivered. No sleeps, no negative timing
//! inference.

#![cfg(unix)]

use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, DriverKind, LaunchOrigin,
    ProviderHealth, ProviderKind, ProviderProfile, pin_binary,
};
use remuda_protocol::{InstanceSpec, ObservationPayload};
use remuda_testing::{
    FakeHerdrOptions, FakeHerdrServer, ShortTempDir, ensure_workspace_bin, install_executable,
};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::time::Duration;

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000011".parse().unwrap(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_closes_emit_a_single_terminal_lifecycle() {
    use remuda_driver::test_faults::TranscriptFaults;

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
    fs::write(&transcript, "").unwrap();

    let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();

    let faults = TranscriptFaults::new();
    // Park the first close between drain and reap/terminal emission.
    faults.arm_close_tail_gate();

    let driver = Arc::new(ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-overlap".into(),
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
    }));
    driver.set_test_faults(faults.clone());

    let handle = driver.as_ref().start(spec(&cwd)).await.expect("start");
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

    let terminal_count = Arc::new(AtomicU32::new(0));
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer_count = Arc::clone(&terminal_count);
    let drainer = tokio::spawn(async move {
        while let Some(obs) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(payload) = &obs.body
                && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Lifecycle(payload) = &obs.body
                && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref()
                && native.native_name == "carrier_closed"
            {
                drainer_count.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(30), bound_rx)
        .await
        .expect("transcript never bound")
        .expect("bound oneshot closed without firing");

    // First close runs to the tail gate and parks there.
    let first_driver = Arc::clone(&driver);
    let first = tokio::spawn(async move { first_driver.close().await });
    assert!(
        faults.wait_close_tail_entered(),
        "the first close never reached its tail park point"
    );

    // Second close (the purge racing the queued instance.close). The barrier
    // proves it has reached the close entry; it cannot pass the close-phase
    // owner while the first close is parked.
    let second_driver = Arc::clone(&driver);
    let second = tokio::spawn(async move { second_driver.close().await });
    assert!(
        faults.wait_close_phase_calls(2),
        "the overlapping close never entered the close phase"
    );

    // Open the gate: the owner finishes reap + terminal emission, then the
    // queued close settles as an idempotent no-op.
    faults.release_close_tail();

    let first_result = tokio::time::timeout(Duration::from_secs(60), first)
        .await
        .expect("first close never finished")
        .expect("first close task panicked");
    first_result.expect("the owning close succeeds");
    let second_result = tokio::time::timeout(Duration::from_secs(60), second)
        .await
        .expect("second close never finished")
        .expect("second close task panicked");
    second_result.expect("the queued close succeeds as a no-op");

    // Channel closure proves every sender retired; and exactly one terminal
    // lifecycle was delivered across both closes.
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed")
        .expect("event drainer panicked");
    assert_eq!(
        terminal_count.load(AtomicOrdering::SeqCst),
        1,
        "two overlapping closes must emit exactly one carrier_closed"
    );

    // A later close stays idempotent and silent.
    driver.close().await.expect("a third close is harmless");
    assert_eq!(
        terminal_count.load(AtomicOrdering::SeqCst),
        1,
        "the later close emits no additional terminal lifecycle"
    );
}
