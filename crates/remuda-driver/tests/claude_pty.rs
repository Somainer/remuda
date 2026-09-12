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
use remuda_testing::{FakeHerdrOptions, FakeHerdrServer, fake_herdr_bin};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn ensure_fake_herdr_bin() -> PathBuf {
    let cargo = env!("CARGO");
    let status = Command::new(cargo)
        .args([
            "build",
            "-p",
            "remuda-testing",
            "--bin",
            "fake-herdr",
            "--quiet",
        ])
        .status()
        .expect("cargo build fake-herdr");
    assert!(status.success(), "failed to build fake-herdr");
    let hinted = fake_herdr_bin();
    if hinted.exists() {
        return hinted;
    }
    let root = workspace_root();
    for profile in ["debug", "release"] {
        let candidate = root.join("target").join(profile).join("fake-herdr");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!("fake-herdr binary not found");
}

fn stub_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    fs::write(&path, "#!/bin/sh\necho '2.1.268 (Claude Code)'\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
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
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-test".into(),
        socket_dir: Some(socket_dir),
        herdr_binary: Some(fake_bin),
        broker: std::sync::Arc::new(remuda_driver::EnvFileSecretBroker),
        extra_env: Default::default(),
        setting_sources: None,
        agent_start_timeout_ms: 5_000,
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
        profile: profile(),
        launch_dir: launch,
        native_home: home,
        binary: BinarySource::Command("claude".into()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-test".into(),
        socket_dir: None,
        herdr_binary: None,
        broker: std::sync::Arc::new(remuda_driver::EnvFileSecretBroker),
        extra_env: Default::default(),
        setting_sources: Some(vec!["project".into(), "local".into()]),
        agent_start_timeout_ms: 180_000,
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
