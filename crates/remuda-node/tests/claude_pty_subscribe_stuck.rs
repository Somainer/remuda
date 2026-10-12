//! c-ctxusage r15 item 6: a Herdr that accepts the polite-stop
//! `events.subscribe` connection but NEVER acknowledges it must not pin the
//! close phase. The handshake is bounded: the close aborts with a retryable
//! carrier error, keeps the pane alive, and releases its ownership so another
//! close re-runs the audit (and hits the same handshake again) instead of
//! wedging the instance as "closing".

#![cfg(unix)]

use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, DriverError, DriverKind,
    LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile, pin_binary,
};
use remuda_protocol::{InstanceSpec, ObservationPayload};
use remuda_testing::{
    FakeHerdrOptions, FakeHerdrServer, ShortTempDir, ensure_workspace_bin, install_executable,
};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000010".parse().unwrap(),
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
async fn a_subscribe_handshake_that_never_acks_aborts_retryably_and_releases_close_ownership() {
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
    fs::write(&transcript, "").unwrap();

    // The carrier accepts subscribe connections but never acks them; all
    // other RPCs (ping/snapshot/...) keep working.
    let _fake =
        FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket).subscribe_never_acks()).unwrap();

    // The polite grace is irrelevant here: the subscribe handshake wedges
    // before Ctrl-C is ever sent.
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
        session_name: "remuda-cp-sub-stuck".into(),
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

    let bound_count = Arc::new(AtomicU32::new(0));
    let drainer_count = Arc::clone(&bound_count);
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer = tokio::spawn(async move {
        while let Some(obs) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(payload) = &obs.body
                && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                drainer_count.fetch_add(1, AtomicOrdering::SeqCst);
                let _ = tx.send(());
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(30), bound_rx)
        .await
        .expect("transcript never bound (timed out)")
        .expect("bound oneshot closed without firing");

    // First close: the subscribe handshake must time out bounded and surface
    // a RETRYABLE carrier error — never a success and never a permanent error.
    let started = Instant::now();
    let first = driver.close().await;
    assert!(
        matches!(first, Err(DriverError::CarrierUnavailable(_))),
        "a wedged subscribe handshake must abort retryably, got {first:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the handshake bound must release the close phase promptly, took {:?}",
        started.elapsed()
    );

    // Ownership was released, not terminated: the agent pane is still live on
    // the carrier (no pane.close was ever dispatched).
    let probe = remuda_herdr::Client::connect(&socket).with_timeout(Duration::from_secs(2));
    let snapshot = probe
        .session_snapshot()
        .await
        .expect("other RPCs still served");
    assert!(
        snapshot
            .agents
            .iter()
            .any(|agent| agent.pane_id.starts_with("w1:")),
        "the native agent pane is still alive — the stuck close destroyed nothing"
    );

    // A second close re-enters the whole sequence (fresh audit, fresh
    // handshake) rather than observing a stuck "closing" phase.
    let started = Instant::now();
    let second = driver.close().await;
    assert!(
        matches!(second, Err(DriverError::CarrierUnavailable(_))),
        "a retried close re-runs the handshake and aborts retryably, got {second:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "retried close must not be wedged on the first attempt's ownership"
    );
    let snapshot = probe
        .session_snapshot()
        .await
        .expect("snapshot after retry");
    assert!(
        snapshot
            .agents
            .iter()
            .any(|agent| agent.pane_id.starts_with("w1:")),
        "the pane still survives the retried stuck close"
    );

    drop(driver);
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed")
        .expect("event drainer panicked");
    assert_eq!(bound_count.load(AtomicOrdering::SeqCst), 1);
}
