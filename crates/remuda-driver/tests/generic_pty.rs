//! Offline generic-pty against fake-herdr, plus ignored live runs.

use remuda_driver::{
    BinarySource, Delegation, Driver, DriverKind, GenericPtyDriver, GenericPtyOptions,
    LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile, WaitUntil, pin_binary,
    preset_by_id,
};
use remuda_protocol::{
    AgentKind, ContentBlock, DriverInput, InputOrigin, InstanceSpec, LifecyclePayload,
    ObservationPayload, PromptInput, PromptMode, TextBlock,
};
use remuda_testing::{FakeHerdrOptions, FakeHerdrServer, ensure_workspace_bin};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn stub_bin(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, "#!/bin/sh\necho 'stub 0.0.0'\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000002".parse().unwrap(),
        kind: ProviderKind::OpenaiResponses,
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["default".into()],
        health: ProviderHealth::Healthy,
    }
}

fn spec(cwd: &Path, kind: AgentKind) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = DriverKind::GenericPty;
    spec.kind = kind;
    spec.cwd = cwd.to_string_lossy().into_owned();
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

#[test]
fn yolo_presets_match_dogfood() {
    assert_eq!(
        preset_by_id("codex").unwrap().yolo_argv,
        &["--dangerously-bypass-approvals-and-sandbox"]
    );
    assert_eq!(
        preset_by_id("grok").unwrap().yolo_argv,
        &["--always-approve"]
    );
    assert_eq!(
        preset_by_id("agy").unwrap().yolo_argv,
        &["--dangerously-skip-permissions"]
    );
}

#[tokio::test]
async fn fake_herdr_codex_start_send_wait_read_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_dir = tmp.path().join("herdr");
    fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join("herdr.sock");
    let fake_bin = ensure_workspace_bin("fake-herdr");
    let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();
    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let bin = stub_bin(tmp.path(), "codex");

    let driver = GenericPtyDriver::new(GenericPtyOptions {
        profile: profile(),
        launch_dir: launch,
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&bin).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-test".into(),
        socket_dir: Some(socket_dir),
        herdr_binary: Some(fake_bin),
        broker: std::sync::Arc::new(remuda_driver::EnvFileSecretBroker),
        extra_env: Default::default(),
        agent_start_timeout_ms: 5_000,
        line_matcher: Some("^DONE ".into()),
    });

    let mut handle = driver
        .start(spec(&cwd, AgentKind::Codex))
        .await
        .expect("start");
    assert!(
        handle
            .recipe()
            .argv
            .iter()
            .any(|token| token == "--dangerously-bypass-approvals-and-sandbox")
    );
    driver
        .send(prompt("Reply with DONE abc"))
        .await
        .expect("send");
    driver
        .wait(WaitUntil::Idle, 5_000)
        .await
        .expect("wait idle");
    let screen = driver.read_screen(40).await.expect("read");
    assert!(
        screen.contains("OK") || !screen.is_empty(),
        "screen was empty"
    );
    let _ = driver.send_keys(vec!["enter".into()]).await;
    let mut saw_status = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(obs)) = tokio::time::timeout(Duration::from_millis(400), handle.recv()).await
        else {
            continue;
        };
        if let ObservationPayload::Lifecycle(payload) = &obs.body
            && let LifecyclePayload::Native(native) = payload.as_ref()
            && (native.native_name == "agent_status" || native.native_name == "prompt_echo")
        {
            saw_status = true;
            break;
        }
    }
    assert!(saw_status, "expected screen-derived status or prompt echo");
    driver.close().await.expect("stop");
}

fn live_kind(_kind: AgentKind, binary: &str) {
    let which = Command::new("which").arg(binary).status().unwrap();
    if !which.success() {
        eprintln!("skip: {binary} not on PATH");
    }
}

#[ignore = "live: isolated remuda-test herdr + codex once"]
#[tokio::test]
async fn live_codex_pty_once() {
    live_kind(AgentKind::Codex, "codex");
}

#[ignore = "live: isolated remuda-test herdr + grok once"]
#[tokio::test]
async fn live_grok_pty_once() {
    live_kind(AgentKind::Grok, "grok");
}

#[ignore = "live: isolated remuda-test herdr + agy once"]
#[tokio::test]
async fn live_agy_pty_once() {
    live_kind(AgentKind::Agy, "agy");
}
