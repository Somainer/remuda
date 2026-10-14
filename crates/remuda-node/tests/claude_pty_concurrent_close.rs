//! c-ctxusage r25 item 2: concurrent closes must serialize the WHOLE close —
//! carrier close, launch cleanup, terminal emission and live-state retirement
//! — so exactly ONE `carrier_closed` terminal observation is emitted and the
//! live state retired once, even when two close() calls race.

#![cfg(unix)]

use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, DriverKind, FileLifetime,
    LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile, pin_binary,
};
use remuda_protocol::{InstanceSpec, LifecyclePayload, ObservationPayload};
use remuda_testing::{
    FakeHerdrOptions, FakeHerdrServer, ShortTempDir, ensure_workspace_bin, install_executable,
};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
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
async fn two_concurrent_closes_emit_one_terminal_observation_and_retire_once() {
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
    fs::write(launch.join("transcript.jsonl"), "").unwrap();

    let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();

    let driver = Arc::new(ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-concurrentclose".into(),
        socket_dir: Some(socket_dir),
        herdr_binary: Some(fake_bin),
        broker: Arc::new(remuda_driver::EnvFileSecretBroker::env_only()),
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

    let handle = driver.start(spec(&cwd)).await.expect("start");
    // c-ctxusage r26 item 4: real launch-lifetime recipe paths, all of which
    // the single completion phase must remove exactly once.
    let launch_files: Vec<std::path::PathBuf> = handle
        .recipe()
        .materialized_files
        .iter()
        .filter(|file| file.lifetime == FileLifetime::Launch)
        .map(|file| std::path::PathBuf::from(&file.path))
        .collect();
    assert!(!launch_files.is_empty());
    for path in &launch_files {
        assert!(path.exists(), "{} exists before close", path.display());
    }
    let mut events_rx = handle.into_events();
    let closed_count = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&closed_count);
    let drainer = tokio::spawn(async move {
        while let Some(obs) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &obs.body
                && let LifecyclePayload::Native(n) = lc.as_ref()
                && n.native_name == "carrier_closed"
            {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    // Fire two closes concurrently. Both must succeed but only one runs the
    // terminal phases (the other waits on close_lock and returns the no-op).
    let d1 = Arc::clone(&driver);
    let c1 = tokio::spawn(async move { d1.close().await });
    let d2 = Arc::clone(&driver);
    let c2 = tokio::spawn(async move { d2.close().await });
    let r1 = tokio::time::timeout(Duration::from_secs(30), c1)
        .await
        .expect("close 1 hung")
        .expect("close 1 task panicked");
    let r2 = tokio::time::timeout(Duration::from_secs(30), c2)
        .await
        .expect("close 2 hung")
        .expect("close 2 task panicked");
    r1.expect("close 1 ok");
    r2.expect("close 2 ok");

    // Let the terminal frame land, then stop the drainer (main retains the
    // sender; channel closure is not the completion witness).
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        closed_count.load(Ordering::SeqCst),
        1,
        "exactly one carrier_closed across two concurrent closes"
    );
    // r26 item 4: the launch cleanup and live-state retirement also happen
    // exactly once, witnessed independently of the terminal frame.
    for path in &launch_files {
        assert!(
            !path.exists(),
            "{} removed by the one completion phase",
            path.display()
        );
    }
    assert!(
        driver.native_ref().await.is_none(),
        "the live state is retired after the concurrent closes"
    );
    assert_eq!(
        driver.retirement_count(),
        1,
        "two concurrent closes retire the captured live state exactly once"
    );
    drainer.abort();
}
