//! c-ctxusage r17 item 7: the polite shell-exit `pane.send_text("exit")` is an
//! OPTIONAL courtesy on a held verified Herdr connection. When the agent has
//! already left and the pane is back at its root shell, the closer tells that
//! shell to exit; a live-boot API refusal of that one send (the pane vanished
//! between the process probe and the send) used to abort the whole interrupt —
//! mapped through `uncertain_close` to a propagated error — so the force path
//! and the reap never ran and the close reported failure even though every
//! DESTRUCTIVE call could still proceed. Only a lost/stalled connection may
//! make the close uncertain; an ordinary API error on the courtesy send is
//! skipped and the close continues: verified stop, reap, terminal emission.

#![cfg(unix)]

use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, DriverKind, LaunchOrigin,
    ProviderHealth, ProviderKind, ProviderProfile, pin_binary,
};
use remuda_protocol::InstanceSpec;
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

fn interrupted_record() -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": "cp-shellexit-record",
        "timestamp": "2026-10-13T00:01:00.000Z",
        "isSidechain": false,
        "message": {
            "id": "msg_shell_exit_usage",
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": null,
            "content": [{"type": "text", "text": "SHELL_EXIT"}],
            "usage": {
                "input_tokens": 303,
                "cache_read_input_tokens": 404,
                "cache_creation_input_tokens": 2,
                "output_tokens": 7
            }
        }
    });
    format!("{record}\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_optional_shell_exit_send_does_not_abort_the_close() {
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
    let interrupt_record_file = launch.join("interrupted-record.jsonl");
    fs::write(&interrupt_record_file, interrupted_record()).unwrap();

    let _fake = FakeHerdrServer::spawn(
        FakeHerdrOptions::new(&socket)
            .interrupt_appends_record(&transcript, &interrupt_record_file)
            .refuse_shell_exit_send_text(),
    )
    .unwrap();

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        memory_context: None,
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-shellexit".into(),
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
    let events_rx = handle.into_events();
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
    // Receive in the background so a full 64-slot buffer can never stall the
    // close-time terminal emission (the driver keeps the sender for the
    // process lifetime under ruling D; the test does not assert channel
    // closure, only that the refused courtesy send did not abort the close).
    let drainer = tokio::spawn(async move {
        let mut events_rx = events_rx;
        while let Some(_observation) = events_rx.recv().await {
            // drain
        }
    });

    // Ctrl+C terminates the agent (fake model): the pane reverts to its root
    // shell, so the polite close probes Gone and attempts the optional
    // `exit\r` send — which the fake refuses with a live-boot pane_not_found.
    // The close must swallow that courtesy refusal, finish the verified stop,
    // reap the pane/tab/workspace.
    tokio::time::timeout(Duration::from_secs(45), driver.close())
        .await
        .expect("close hung past its bound")
        .expect(
            "a non-conn-lost refusal of the optional shell-exit send must not \
             abort the interrupt or skip reap",
        );
    // The close completed through interrupt + reap; stop the drainer (the
    // sender is intentionally retained) to finish the test cleanly.
    drainer.abort();

    // Idempotent retry: the reaped close settles as a clean no-op, not an
    // error against resources the first (swallowed-error) close already
    // removed.
    tokio::time::timeout(Duration::from_secs(15), driver.close())
        .await
        .expect("the retried close hung")
        .expect("a retried close after reaping is a clean no-op");
}
