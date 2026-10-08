//! c-ctxusage r5 item 2 (ShellPtyDriver::close leg): drive the REAL
//! ShellPtyDriver through start -> send -> close over a seeded fake-harness
//! transcript. The promotion poller's cooperative shutdown (watch shutdown +
//! finalised oneshot) must tear the live agent down cleanly without hanging
//! or aborting mid finalise, and the turn's usage observations emitted by the
//! promoter loop must reach the RunHandle channel with the scenario's split
//! cache-bucket counters.
//!
//! The mapper-level "assistant record with usage + null stop_reason ->
//! finish() publishes on close" semantic is covered by the carrier unit test
//! `hydrator_finalise_publishes_usage_without_stop_reason_on_close`; the
//! observation -> Hub `project_usage_event` + `insert_usage_event` leg is
//! covered over the real Hub HTTP API by `usage_native_rollup.rs`. This test
//! is the production close/plumbing leg between them.
#![cfg(unix)]

use remuda_driver::shell_pty::{AgentLaunch, HookConfig, ShellPtyDriver, ShellPtyOptions};
use remuda_driver::{
    Delegation, Driver, DriverKind, LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_protocol::{
    AgentKind, ContentBlock, DriverInput, InstanceSpec, ObservationPayload, PromptInput, TextBlock,
    U64,
};
use remuda_testing::ensure_workspace_bin;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

fn relay_bin() -> PathBuf {
    if let Ok(path) = std::env::var("REMUDA_NATIVE_LIVE_RELAY") {
        return PathBuf::from(path);
    }
    let deps = std::env::current_exe().unwrap();
    let deps = deps.parent().unwrap();
    for candidate in [deps.join("remuda"), deps.join("../remuda")] {
        if candidate.is_file() {
            return candidate;
        }
    }
    remuda_testing::workspace_root().join("target/debug/remuda")
}

fn spec_for(cwd: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec = serde_json::from_str(include_str!(
        "../../remuda-driver/tests/fixtures/instance-spec.json"
    ))
    .unwrap();
    spec.driver = DriverKind::ShellPty;
    spec.kind = AgentKind::Claude;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("haiku".into());
    spec
}

fn prompt(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: remuda_protocol::PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: remuda_protocol::InputOrigin::Human,
        native_client_message_id: "probe".into(),
    }))
}

fn scenario() -> String {
    r#"{
  "turns": [{
    "match_prefix": "CLOSEUSAGE",
    "text": "CLOSEUSAGE_DONE",
    "chunks": 1,
    "tools": [{
      "name": "Bash",
      "input": { "command": "sleep 3" },
      "approval": "auto",
      "duration_ms": 3000,
      "exit_code": 0
    }],
    "stop_reason": "end_turn",
    "usage": {
      "input_tokens": 100,
      "output_tokens": 11,
      "cached_tokens": 200,
      "cache_creation_5m": 7,
      "cache_creation_1h": 3
    }
  }]
}"#
    .to_string()
}

fn install_fake_claude(root: &Path) -> PathBuf {
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).unwrap();
    let dest = install.join("claude");
    std::fs::copy(ensure_workspace_bin("fake-harness"), &dest).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    dest
}

fn known_u64(knowledge: &remuda_protocol::Knowledge<U64>) -> i64 {
    match knowledge {
        remuda_protocol::Knowledge::Known { value: U64(n) } => *n as i64,
        _ => 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_pty_close_shuts_down_cooperatively_and_emits_the_turn_usage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let workspace = root.join("workspace");
    let native_home = root.join("home");
    let instance_dir = root.join("instance");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&native_home).unwrap();
    let binary = install_fake_claude(root);
    let scenario_path = root.join("scenario.json");
    std::fs::write(&scenario_path, scenario()).unwrap();
    let events_path = root.join("harness-events.jsonl");

    let mut options = ShellPtyOptions::agent(
        workspace.clone(),
        AgentKind::Claude,
        AgentLaunch {
            profile: Box::new(profile()),
            launch_dir: instance_dir.join("launch"),
            native_home: native_home.clone(),
            binary: Some(binary),
            origin: LaunchOrigin::Human,
            native_home_managed: true,
            settings_overlay: None,
        },
    );
    options.emulator = true;
    options
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());
    options.extra_env.insert(
        "FAKE_HARNESS_SCRIPT".into(),
        scenario_path.to_string_lossy().into_owned(),
    );
    options.extra_env.insert(
        "FAKE_HARNESS_EVENTS_OUT".into(),
        events_path.to_string_lossy().into_owned(),
    );
    options.hooks = Some(HookConfig {
        instance_dir,
        relay_binary: relay_bin(),
        tui: remuda_driver::TuiMode::Default,
    });
    let driver = ShellPtyDriver::new(options);

    let handle = driver.start(spec_for(&workspace)).await.expect("start");
    let mut events_rx = handle.into_events();

    let deadline = Instant::now() + Duration::from_secs(25);
    let mut ready = false;
    while Instant::now() < deadline {
        if Driver::wait_control(&driver).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "composer never became ready");

    // A short settle after control opens before writing the prompt (the
    // emulated screen needs a beat to reach the input state).
    tokio::time::sleep(Duration::from_millis(500)).await;

    driver.send(prompt("CLOSEUSAGE probe")).await.expect("send");

    // Let the turn complete so its assistant usage record is written and the
    // promoter hydrates it; then close. The r5 item 6 guarantee is that close
    // itself still finalises whatever the last pump left buffered — here the
    // completed turn's usage, delivered cleanly rather than aborted.
    tokio::time::sleep(Duration::from_millis(4500)).await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let close_started = Instant::now();
    driver.close().await.expect("close");
    assert!(
        close_started.elapsed() < Duration::from_secs(15),
        "close took the abort-timeout path instead of cooperative finalise"
    );

    // Drain whatever the promoter emitted, for a bounded window after close.
    let observations = tokio::time::timeout(Duration::from_secs(3), async {
        let mut out = Vec::new();
        while let Ok(obs) = tokio::time::timeout(Duration::from_millis(500), events_rx.recv()).await
        {
            out.push(obs);
        }
        out
    })
    .await
    .expect("drain window");
    let flat: Vec<_> = observations.into_iter().flatten().collect();
    let usage: Vec<_> = flat
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::Usage(payload) => Some(payload.as_ref()),
            _ => None,
        })
        .collect();
    if usage.is_empty() {
        let kinds: Vec<String> = flat
            .iter()
            .map(|obs| {
                serde_json::to_string(&obs.body)
                    .unwrap_or_default()
                    .chars()
                    .take(60)
                    .collect()
            })
            .collect();
        eprintln!(
            "OBSERVATIONS AROUND CLOSE ({n}): {kinds:?}",
            n = kinds.len()
        );
        let events = std::fs::read_to_string(root.join("harness-events.jsonl")).unwrap_or_default();
        eprintln!("HARNESS EVENTS:\n{events}");
    }
    assert!(
        !usage.is_empty(),
        "the promotion loop emitted turn usage around close"
    );
    // Each emitted scope id carries the scenario counters (the Hub upserts
    // revisions by scope_id; sum across the distinct messages).
    let sums = usage.iter().fold((0, 0, 0, 0), |acc, payload| {
        (
            acc.0 + known_u64(&payload.input_tokens),
            acc.1 + known_u64(&payload.output_tokens),
            acc.2 + known_u64(&payload.cache_read_tokens),
            acc.3 + known_u64(&payload.cache_write_tokens),
        )
    });
    let scope_ids: Vec<_> = usage.iter().map(|p| p.scope_id.as_str()).collect();
    assert_eq!(
        sums,
        (200, 22, 400, 20),
        "two messages * (input 100, output 11, read 200, write 10): {scope_ids:?}"
    );
}
