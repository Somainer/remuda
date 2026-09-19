//! Offline `claude-pty` start→prompt→idle→close against fake-herdr.
//!
//! Fixture source: `crates/remuda-testing/fixtures/herdr/` (live herdr 0.9.0 capture).

use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, DriverKind, LaunchOrigin,
    ProviderHealth, ProviderKind, ProviderProfile, pin_binary,
};
use remuda_protocol::{
    Completeness, ContentBlock, DriverInput, InputOrigin, InstanceSpec, Knowledge,
    LifecyclePayload, ObservationPayload, PromptInput, PromptMode, TextBlock,
};
use remuda_testing::{FakeHerdrOptions, FakeHerdrServer, ensure_workspace_bin, install_executable};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use base64::Engine as _;

fn ensure_fake_herdr_bin() -> PathBuf {
    ensure_workspace_bin("fake-herdr")
}

fn stub_claude(dir: &Path) -> PathBuf {
    install_executable(dir, "claude", "#!/bin/sh\necho '2.1.268 (Claude Code)'\n")
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn spec(cwd: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = DriverKind::ClaudePty;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("haiku".into());
    spec
}

fn prompt(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "m1".into(),
    }))
}

#[tokio::test]
async fn fake_herdr_start_prompt_idle_close() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().join("herdr");
    fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join("herdr.sock");
    let fake_bin = ensure_fake_herdr_bin();
    let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();

    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let claude = stub_claude(tmp.path());

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-test".into(),
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

    let mut handle = driver.start(spec(&cwd)).await.expect("start");
    let recipe = handle.recipe();
    assert!(
        !recipe.argv.iter().any(|token| token == "--bare"),
        "argv must never include --bare: {:?}",
        recipe.argv
    );
    assert!(
        recipe.argv.iter().any(|token| token == "--settings"),
        "SessionStart hook overlay must be referenced by --settings"
    );
    assert!(launch.join("session-start.sh").is_file());
    let json = serde_json::to_string(recipe).unwrap();
    assert!(!json.contains("sk-"));

    let first = tokio::time::timeout(Duration::from_secs(3), handle.recv())
        .await
        .expect("lifecycle timeout")
        .expect("lifecycle event");
    assert!(matches!(first.body, ObservationPayload::Lifecycle(_)));

    // Herdr's idle alone is insufficient: the real Claude trust transition can
    // report idle before the prompt composer exists. Simulate the native hook.
    assert!(matches!(
        driver.wait_control().await,
        Err(remuda_driver::DriverError::ControlUnavailable)
    ));
    assert!(matches!(
        driver
            .send(prompt("not dispatched before SessionStart"))
            .await,
        Err(remuda_driver::DriverError::ControlUnavailable)
    ));
    fs::write(
        launch.join("session-meta.json"),
        serde_json::json!({
            "session_id": "fixture-session",
            "transcript_path": launch.join("transcript.jsonl"),
            "hook_event_name": "SessionStart",
        })
        .to_string(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while driver.wait_control().await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    driver
        .send(prompt("Reply with exactly OK"))
        .await
        .expect("prompt");

    let mut saw_idle = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(800), handle.recv()).await;
        let Ok(Some(obs)) = next else {
            continue;
        };
        if let ObservationPayload::Lifecycle(payload) = &obs.body
            && let LifecyclePayload::Native(native) = payload.as_ref()
            && let Knowledge::Known { value } = &native.status
            && value == "idle"
        {
            saw_idle = true;
            break;
        }
    }
    assert!(saw_idle, "expected idle agent_status observation");

    driver.close().await.expect("close");
    let snapshot = remuda_herdr::Client::connect(&socket)
        .session_snapshot()
        .await
        .unwrap();
    assert!(
        snapshot.panes.is_empty(),
        "stop must reclaim both agent and root shell"
    );
    assert!(snapshot.tabs.is_empty(), "stop must close the tab");
    assert!(
        snapshot.workspaces.is_empty(),
        "stop must close the workspace"
    );
    let mut exited = false;
    while let Ok(Some(observation)) =
        tokio::time::timeout(Duration::from_millis(100), handle.recv()).await
    {
        if let ObservationPayload::Lifecycle(payload) = observation.body
            && let LifecyclePayload::Native(native) = *payload
            && native.native_name == "carrier_closed"
        {
            assert!(matches!(native.status, Knowledge::Known { value } if value == "exited"));
            exited = true;
            break;
        }
    }
    assert!(exited, "close must emit an exit observation");
    driver.close().await.expect("idempotent close");
}

/// Build the three-frame relay script: inline paint, a scripted enter of the
/// DEC alt screen (`?1049h`), then a leave (`?1049l`). Sleep directives make
/// each mode observable before the next flip.
fn write_alt_screen_frames(path: &Path) {
    let frame = |seq: u64, full: bool, bytes: &[u8]| {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        serde_json::json!({
            "type": "terminal.frame",
            "seq": seq,
            "encoding": "ansi",
            "width": 80,
            "height": 24,
            "full": full,
            "bytes": encoded,
        })
        .to_string()
    };
    let body = [
        frame(1, true, b"inline fake harness\r\n$ "),
        "# sleep-ms 800".to_string(),
        frame(
            2,
            false,
            b"\x1b[?1049h\x1b[2J\x1b[Hfullscreen fake harness\r\n",
        ),
        "# sleep-ms 800".to_string(),
        frame(3, false, b"\x1b[?1049lback inline\r\n$ "),
    ]
    .join("\n")
        + "\n";
    fs::write(path, body).unwrap();
}

async fn poll_alt_screen(driver: &ClaudePtyDriver, want: Option<bool>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        if driver.alt_screen().await == want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "alt-screen did not reach {want:?} (last: {:?})",
            driver.alt_screen().await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The herdr relay tracks alt-screen deterministically from the pane byte
/// stream: before any relay has attached there is no observation (`None`);
/// once attached the badge data is `Some(false)` (inline), flips to
/// `Some(true)` when the harness enters the alt screen, and returns to
/// `Some(false)` when it leaves. No herdr mode API is involved — the fake
/// relay just plays frames containing the DEC sequences.
#[tokio::test]
async fn claude_pty_alt_screen_flips_when_the_harness_enters_fullscreen() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().join("herdr");
    fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join("herdr.sock");
    let fake_bin = ensure_fake_herdr_bin();

    // The relay subprocess discovers `<socket>.frames` next to the API
    // socket, so point the server at the same scripted file.
    let frames_path = socket_dir.join("herdr.sock.frames");
    write_alt_screen_frames(&frames_path);

    let mut options = FakeHerdrOptions::new(&socket);
    options.frames = frames_path.clone();
    let _fake = FakeHerdrServer::spawn(options).unwrap();

    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let claude = stub_claude(tmp.path());

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch,
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-test".into(),
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

    let _handle = driver.start(spec(&cwd)).await.expect("start");

    // No relay has attached yet: the carrier honestly reports "unknown".
    assert_eq!(driver.alt_screen().await, None);

    let native_ref = driver.native_ref().await.expect("native ref");
    driver.attach(native_ref).await.expect("attach");

    // First frame is the inline shell paint.
    poll_alt_screen(&driver, Some(false)).await;
    // The scripted harness switches to `?1049`.
    poll_alt_screen(&driver, Some(true)).await;
    // And leaves it again.
    poll_alt_screen(&driver, Some(false)).await;

    driver.close().await.expect("close");
}

#[ignore = "live: isolated remuda-test herdr session, claude --model haiku once"]
#[tokio::test]
async fn live_claude_pty_haiku_once() {
    let which = Command::new("which").arg("claude").status().unwrap();
    if !which.success() {
        eprintln!("skip: claude not on PATH");
        return;
    }
    let which_herdr = Command::new("which").arg("herdr").status().unwrap();
    if !which_herdr.success() {
        eprintln!("skip: herdr not on PATH");
        return;
    }

    let root = PathBuf::from("/tmp/remuda-driver");
    let cwd = root.join("pty-live");
    fs::create_dir_all(&cwd).unwrap();
    let launch = root.join("pty-launch");
    let home = root.join("pty-home");
    fs::create_dir_all(&home).unwrap();

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch,
        native_home: home,
        binary: BinarySource::Command("claude".into()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-test".into(),
        socket_dir: None,
        herdr_binary: None,
        broker: std::sync::Arc::new(remuda_driver::EnvFileSecretBroker::env_only()),
        extra_env: Default::default(),
        agent_mcp: None,
        setting_sources: Some(vec!["project".into(), "local".into()]),
        agent_start_timeout_ms: 180_000,
        inherit_default_config: false,
        settings_overlay_path: None,
        auto_trust_registered_workspace: false,
        seed_onboarding: true,
        host_claude_config: None,
    });
    let mut spec = spec(&cwd);
    spec.model_id = Some("haiku".into());
    spec.args = vec!["--max-budget-usd".into(), "0.3".into()];
    let mut handle = driver.start(spec).await.expect("live start");
    driver
        .send(prompt(
            "Reply with exactly the word OK. Do not use any tools.",
        ))
        .await
        .expect("live prompt");
    let mut idle = false;
    for _ in 0..60 {
        if let Ok(Some(obs)) = tokio::time::timeout(Duration::from_secs(2), handle.recv()).await
            && let ObservationPayload::Lifecycle(payload) = &obs.body
            && let LifecyclePayload::Native(native) = payload.as_ref()
            && let Knowledge::Known { value } = &native.status
            && value == "idle"
        {
            idle = true;
            break;
        }
    }
    assert!(idle, "live claude-pty did not reach idle");
    let _ = Completeness::Structured;
    driver.close().await.expect("live close");
}
