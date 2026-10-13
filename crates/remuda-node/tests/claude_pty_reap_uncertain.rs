//! c-ctxusage r16 item 1 (F1'): EVERY destructive Herdr call on EVERY close
//! path runs on the SAME held-open connection that served that phase's
//! ownership audit — not only the forced pane.close (r15), but the reap
//! phase's pane/tab/workspace closes after EITHER a polite exit or a
//! successful force.
//!
//! Herdr allocates ids deterministically (`w1`, `w1:t1`, `w1:p1`, …), so a
//! Herdr process replaced in the gap between the reap audit and a pane.close
//! hands the reused ids to an unrelated workspace. A reap that audits and
//! closes on different (one-shot) connections dispatches its close into the
//! replacement boot and destroys the stranger's resources. The fake's
//! restart barrier (ordinal-selectable: the first close models the
//! polite-then-reap gap, the second close the force-then-reap gap) swaps the
//! state to the foreign-labelled reuse and drops a connection that carried an
//! audit snapshot WITHOUT processing its close; a one-shot close connection
//! instead has the close processed against the foreign boot.
//!
//! Both tests assert: the first close is reported CloseOutcomeUncertain, the
//! foreign workspace, BOTH its panes and its live agent survive, and a
//! retried close is a harmless no-op that leaves the stranger untouched.

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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::time::Duration;

const FOREIGN_LABEL: &str = "unrelated-workspace-after-restart";
/// The interrupted/final record appended to the transcript by the fake
/// writer when it is stopped.
const STOP_RECORD: &str = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"x\"}}\n";

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000016".parse().unwrap(),
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

/// Prove the foreign-labelled replacement boot survived an uncertain close:
/// generation bumped, both reused panes (`w1:p1` root, `w1:p2` foreign
/// agent) live, and the foreign agent itself present.
async fn assert_foreign_survives(socket: &Path) {
    let probe = remuda_herdr::Client::connect(socket).with_timeout(Duration::from_secs(2));
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
        foreign_ws.label, FOREIGN_LABEL,
        "w1 is a foreign workspace now"
    );
    let foreign_pane = snapshot
        .agents
        .iter()
        .find(|agent| agent.name.as_deref() == Some("foreign-agent"))
        .map(|agent| agent.pane_id.clone())
        .expect("the foreign workspace keeps its own live agent");
    assert_eq!(
        foreign_pane, "w1:p2",
        "the predecessor's agent pane id was reused"
    );
    for pane_id in ["w1:p1", "w1:p2"] {
        assert!(
            snapshot.panes.iter().any(|pane| pane.pane_id == pane_id),
            "the foreign pane {pane_id} must survive the aborted reap"
        );
    }
}

/// Which writer-stop model the fake uses before the reap barrier.
#[derive(Clone, Copy)]
enum StopModel {
    /// Ctrl-C appends the final record and terminates: the writer exits
    /// gracefully and the interrupt phase sends NO pane.close. The barrier
    /// therefore fires on the FIRST pane.close overall — the reap's.
    Polite,
    /// Ctrl-C is ignored: the interrupt phase FORCES pane.close (the first
    /// pane.close overall, which must complete), and the barrier fires on the
    /// SECOND pane.close — the reap's.
    Forced,
}

async fn run_reap_barrier(session_name: &str, model: StopModel) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_root = ShortTempDir::new().unwrap();
    let socket_dir = socket_root.as_ref().join("herdr");
    fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join("herdr.sock");
    let fake_bin = ensure_workspace_bin("fake-herdr");

    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let launch: PathBuf = tmp.path().join("launch");
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
    let record_file = tmp.path().join("close-record.jsonl");
    fs::write(&record_file, STOP_RECORD).unwrap();

    let options = FakeHerdrOptions::new(&socket);
    let options = match model {
        StopModel::Polite => options
            .interrupt_appends_record(&transcript, &record_file)
            .restart_barrier_on_close(),
        StopModel::Forced => options
            .force_close_appends_record(&transcript, &record_file)
            .restart_barrier_on_nth_close(2),
    };
    let _fake = FakeHerdrServer::spawn(options).unwrap();

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: session_name.into(),
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

    // The replacement server appears in the reap audit→close gap: the close
    // must be UNCERTAIN, never reported closed.
    let close = driver.close().await;
    assert!(
        matches!(close, Err(DriverError::CloseOutcomeUncertain(_))),
        "the reap close outcome must be uncertain, got {close:?}"
    );

    // The stranger's whole layout survives the aborted reap.
    assert_foreign_survives(&socket).await;

    // A retried close re-audits the replacement boot, finds our workspace
    // gone, and finishes as a no-op; the stranger keeps working.
    driver
        .close()
        .await
        .expect("a retry settles with our resources gone");
    assert_foreign_survives(&socket).await;

    drainer.abort();
    assert_eq!(bound_count.load(AtomicOrdering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn polite_exit_then_restart_in_the_reap_gap_aborts_uncertain_and_spares_the_foreign_pane() {
    // Polite model: the writer leaves on Ctrl-C, so the interrupt phase sends
    // no pane.close; the first pane.close overall is the reap's and the
    // barrier fires there (ordinal 1).
    run_reap_barrier("remuda-cp-reap-polite", StopModel::Polite).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_then_restart_in_the_reap_gap_aborts_uncertain_and_spares_the_foreign_pane() {
    // Delayed-writer model: the interrupt phase's forced pane.close
    // completes (close ordinal 1); the replacement appears only when the reap
    // dispatches its first pane.close (ordinal 2).
    run_reap_barrier("remuda-cp-reap-force", StopModel::Forced).await;
}
