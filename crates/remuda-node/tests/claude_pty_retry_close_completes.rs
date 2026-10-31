//! c-ctxusage r25 item 3: a REAL first carrier-close failure (fake Herdr
//! refuses the close connection's opening snapshot with a transient
//! server_unavailable) surfaces an error WITHOUT completing terminal phases;
//! a retry succeeds and emits exactly ONE terminal observation, runs launch
//! cleanup and retires the live state; a third close is a clean no-op.

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
        id: "pvp_01993ab0-0000-7000-8000-000000000012".parse().unwrap(),
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
async fn a_failed_first_carrier_close_is_completed_once_by_a_retry() {
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
    // Marker that arms the fake's close-subscribe refusal while present.
    let refuse_marker = tmp.path().join("refuse-closes");
    fs::write(&refuse_marker, "1").unwrap();

    // The fake refuses the FIRST close-phase events.subscribe with a
    // transient server_unavailable; the retry connection's snapshot succeeds.
    let _fake = FakeHerdrServer::spawn(
        FakeHerdrOptions::new(&socket).refuse_close_subscribes_while(&refuse_marker),
    )
    .unwrap();

    let driver = Arc::new(ClaudePtyDriver::new(ClaudePtyOptions {
        memory_context: None,
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-realretry".into(),
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
    // c-ctxusage r26 item 4: the ACTUAL launch-lifetime recipe paths
    // (report_launch_cleanup shreds these on a successful close).
    let launch_files: Vec<std::path::PathBuf> = handle
        .recipe()
        .materialized_files
        .iter()
        .filter(|file| file.lifetime == FileLifetime::Launch)
        .map(|file| std::path::PathBuf::from(&file.path))
        .collect();
    assert!(
        !launch_files.is_empty(),
        "the launch materializes at least one launch-lifetime file"
    );
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
    // Deterministically wait until the fake reports BOTH the live idle agent
    // AND the driver's own labeled workspace, so the close ownership audit
    // (which checks workspace label + pane) deterministically returns Ours
    // and reaches the events.subscribe refusal.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let probe = remuda_herdr::Client::connect(&socket).with_timeout(Duration::from_secs(2));
            if let Ok(snapshot) = probe.session_snapshot().await
                && snapshot
                    .workspaces
                    .iter()
                    .any(|w| w.label.starts_with("remuda-"))
                && snapshot
                    .agents
                    .iter()
                    .any(|agent| agent.agent_status == remuda_herdr::AgentStatus::Idle)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("agent/own workspace never ready");

    // First close: the close interrupt's events.subscribe is refused with a
    // transient server_unavailable. No terminal observation yet. A rare early
    // close may return Ok(NotDispatched) if the ownership audit sees the agent
    // as absent (a no-op that emits nothing and consumes no refusal); loop
    // until the refused close is observed (it has not emitted terminal state).
    let first = tokio::time::timeout(Duration::from_secs(30), driver.close())
        .await
        .expect("first close hung");
    assert!(
        matches!(
            first,
            Err(remuda_driver::DriverError::CarrierUnavailable(_))
        ),
        "the first carrier close fails with a transient carrier error, got {first:?}"
    );
    // Disarm the transient refusal so the retry's close phase succeeds.
    let _ = fs::remove_file(&refuse_marker);
    assert_eq!(
        closed_count.load(Ordering::SeqCst),
        0,
        "no carrier_closed on the failed first close"
    );
    // r26 item 4: a FAILED close never runs launch cleanup and never retires
    // the live state — the overlays survive for the retry.
    for path in &launch_files {
        assert!(
            path.exists(),
            "{} must survive the failed first close",
            path.display()
        );
    }
    assert!(
        driver.native_ref().await.is_some(),
        "the failed close does not retire the live state"
    );
    assert_eq!(driver.retirement_count(), 0);

    // Retry: succeeds and completes the terminal phases once. Wait for the
    // single carrier_closed frame bounded, rather than a fixed sleep.
    tokio::time::timeout(Duration::from_secs(30), driver.close())
        .await
        .expect("retry hung")
        .expect("the retry completes");
    let mut terminal_after_retry = 0u32;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        terminal_after_retry = closed_count.load(Ordering::SeqCst);
        if terminal_after_retry >= 1 {
            break;
        }
    }
    assert_eq!(
        terminal_after_retry, 1,
        "exactly one carrier_closed after the successful retry"
    );
    // r26 item 4: the successful retry really ran launch cleanup (every
    // launch-lifetime overlay is gone) and retired the captured live state
    // exactly once.
    for path in &launch_files {
        assert!(
            !path.exists(),
            "{} removed by report_launch_cleanup on the successful close",
            path.display()
        );
    }
    assert!(
        driver.native_ref().await.is_none(),
        "the successful close retires the live state"
    );
    assert_eq!(
        driver.retirement_count(),
        1,
        "the successful retry retires the live state once"
    );

    // A third close is a clean no-op (no second terminal observation).
    tokio::time::timeout(Duration::from_secs(30), driver.close())
        .await
        .expect("third close hung")
        .expect("third close is a no-op");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        closed_count.load(Ordering::SeqCst),
        1,
        "the no-op close does not emit a second carrier_closed"
    );
    assert_eq!(
        driver.retirement_count(),
        1,
        "the no-op close never retires again"
    );

    drainer.abort();
}
