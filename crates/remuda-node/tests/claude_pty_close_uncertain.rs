//! c-ctxusage r15 item 1: a forced close whose verified connection dies in the
//! audit→close gap is ABORTED and reported UNCERTAIN — never "closed", never
//! retried against a replacement server.
//!
//! Herdr allocates ids deterministically (`w1`, `w1:t1`, `w1:p1`, …). If the
//! Herdr process restarts in the gap between the final ownership audit and the
//! destructive `pane.close`, a replacement server has re-bound the socket and
//! handed the same pane id to an unrelated workspace. A client that audits and
//! closes on DIFFERENT connections dispatches the close into the replacement
//! boot and kills the stranger's pane. The driver now audits, closes and
//! verifies on ONE held-open connection; the fake models the restart barrier
//! by swapping its state to the foreign-labelled reuse and dropping a
//! connection that carried the audit snapshot WITHOUT processing its close
//! (the accepted fd dies with the old process). The driver must see the
//! disconnect, report the outcome uncertain, and leave the foreign pane and
//! its live agent in place. A retried close is a harmless no-op.

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
async fn a_restart_between_the_verified_audit_and_close_aborts_uncertain_and_spares_the_foreign_pane()
 {
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
    // Delayed-writer model: ctrl+c neither appends nor terminates, so the
    // agent is live when the grace expires and the forced close path runs.
    let force_record = tmp.path().join("force-record.jsonl");
    fs::write(
        &force_record,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"x\"}}\n",
    )
    .unwrap();

    let _fake = FakeHerdrServer::spawn(
        FakeHerdrOptions::new(&socket)
            .force_close_appends_record(&transcript, &force_record)
            .restart_barrier_on_close(),
    )
    .unwrap();

    // The polite wait is bounded short; the agent ignores ctrl+c anyway.

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        memory_context: None,
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-uncertain".into(),
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
                && native.native_name == "SessionStart"
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

    // The close MUST be reported uncertain: the verified connection died in
    // the audit→pane.close gap, so the destructive call was aborted.
    let close = driver.close().await;
    assert!(
        matches!(close, Err(DriverError::CloseOutcomeUncertain(_))),
        "the close outcome must be uncertain, never closed, got {close:?}"
    );

    // Positive proof the foreign pane, workspace and live agent survived the
    // aborted close, on a server whose boot generation bumped.
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
    let foreign_pane = snapshot
        .agents
        .iter()
        .find(|agent| agent.name.as_deref() == Some("foreign-agent"))
        .map(|agent| agent.pane_id.clone())
        .expect("the foreign workspace has its own live agent");
    assert_eq!(
        foreign_pane, "w1:p2",
        "the predecessor's pane id was reused"
    );
    assert!(
        snapshot
            .panes
            .iter()
            .any(|pane| pane.pane_id == foreign_pane),
        "the foreign pane {foreign_pane} must survive the aborted close"
    );

    // A retried close re-audits the replacement boot: our workspace is gone,
    // so cleanup finishes as a no-op and the stranger keeps working.
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

    drainer.abort();
    assert_eq!(bound_count.load(AtomicOrdering::SeqCst), 1);
}
