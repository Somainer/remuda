//! c-ctxusage r14 item 1: a forced close must not kill a FOREIGN pane.
//!
//! Herdr allocates ids deterministically (`w1`, `w1:t1`, `w1:p1`, …). If Herdr
//! restarts inside the close window and reuses the agent pane id for an
//! unrelated workspace, the driver's forced termination used to send
//! `pane.close` against the reused id — killing the stranger's pane and its
//! live process. Ownership (workspace creation label + server boot
//! generation) is now re-proven immediately before every destructive call,
//! so the close is refused and the foreign pane (and its live agent)
//! survive. A retried close is a harmless no-op against a foreign boot.
//!
//! The fake models the restart deterministically: the first live-agent
//! ctrl+c wipes the predecessor state, bumps `ping`'s endpoint generation,
//! and hands a FOREIGN-labelled workspace its own live agent on the SAME
//! pane id the predecessor's agent used (a split pane, `w1:p2`). No timing
//! inference is involved.

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
use std::time::Duration;

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
async fn a_forced_close_after_a_server_restart_refuses_the_reused_foreign_pane() {
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

    // The fake restarts on the first ctrl+c and reuses the agent pane id for
    // a FOREIGN workspace with its own live agent.
    let _fake =
        FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket).restart_reuses_ids_on_ctrlc())
            .unwrap();

    // The polite exit returns "foreign" the moment the restart lands; the
    // shortened grace only bounds how long the close call runs.
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
        session_name: "remuda-cp-foreign".into(),
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

    // Acknowledgement barrier: the seeded replay bound the transcript; the
    // driver owns w1 (remuda-<uuid> label) on server generation 1.
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

    // The close MUST be refused: the reused agent pane now belongs to another
    // boot's unrelated workspace.
    let close = driver.close().await;
    assert!(
        matches!(close, Err(DriverError::ForeignHerdrResource(_))),
        "the forced close must refuse the reused foreign pane, got {close:?}"
    );

    // Positive proof the foreign pane, workspace and live agent survived the
    // refused close, on a server whose boot generation bumped.
    let probe = remuda_herdr::Client::connect(&socket).with_timeout(Duration::from_secs(2));
    let generation = probe
        .ping()
        .await
        .expect("ping")
        .capabilities
        .and_then(|caps| caps.endpoint_protocol_generation);
    assert_eq!(generation, Some(2), "the fake server restarted");
    let snapshot = probe.session_snapshot().await.expect("snapshot");
    let foreign_ws = snapshot
        .workspaces
        .iter()
        .find(|w| w.workspace_id == "w1")
        .expect("the reused workspace id exists");
    assert_eq!(
        foreign_ws.label, "unrelated-workspace-after-restart",
        "w1 is now an unrelated workspace, not ours"
    );
    // The predecessor's agent lived on its split pane; that exact id is the
    // one the new boot hands its own agent.
    let foreign_pane = snapshot
        .agents
        .iter()
        .find(|agent| agent.name.as_deref() == Some("foreign-agent"))
        .map(|agent| agent.pane_id.clone())
        .expect("the foreign workspace has its own live agent");
    assert_eq!(
        foreign_pane, "w1:p2",
        "the predecessor's agent pane id was reused"
    );
    assert!(
        snapshot
            .panes
            .iter()
            .any(|pane| pane.pane_id == foreign_pane),
        "the foreign pane {foreign_pane} must survive the refused close"
    );

    // A retried close must still never touch the foreign pane: our workspace
    // is simply gone, so cleanup finishes as a no-op while the stranger keeps
    // working.
    driver
        .close()
        .await
        .expect("a retry settles with our resources gone");
    let snapshot = probe
        .session_snapshot()
        .await
        .expect("snapshot after retry");
    assert!(
        snapshot
            .panes
            .iter()
            .any(|pane| pane.pane_id == foreign_pane),
        "the foreign pane must still survive the retried close"
    );
    assert!(
        snapshot
            .agents
            .iter()
            .any(|agent| agent.pane_id == foreign_pane),
        "the foreign agent must still survive the retried close"
    );

    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed")
        .expect("event drainer panicked");
    assert_eq!(bound_count.load(AtomicOrdering::SeqCst), 1);
}
