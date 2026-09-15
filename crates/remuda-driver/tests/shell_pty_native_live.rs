//! Live reproduction of D-028 native-carrier-3 findings against the REAL claude
//! binary on this host (ignored by default; `cargo test -- --ignored`).
//!
//! It launches claude through `ShellPtyDriver` exactly the way the Node does
//! under `REMUDA_PTY_CARRIER=native REMUDA_PTY_EMULATOR=1 REMUDA_PTY_HOOKS=1`,
//! dumps the emulator screen (the same rendered grid the Node holds) at fixed
//! intervals, sends the PONG probe once the composer is ready, and reports the
//! observation channels that actually fired.
//!
//! Two scenarios, selected by env:
//! * `baseline` (default) — production wiring as of the macOS retest: claude
//!   inherits the host `HOME`, cwd is a fresh untrusted directory;
//! * `fixed` — onboarding+trust pre-seeded native home, CLAUDE_CONFIG_DIR
//!   pinned to it.
//!
//! `REMUDA_NATIVE_LIVE_RELAY` overrides the `remuda` binary used as hook relay;
//! it defaults to `<target dir>/debug/remuda` next to this test binary.

#![cfg(unix)]

use remuda_driver::shell_pty::{AgentLaunch, HookConfig, ShellPtyDriver, ShellPtyOptions};
use remuda_driver::{
    Delegation, Driver, DriverKind, LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_protocol::{
    AgentKind, ClaudeInteractionMode, ClaudePermission, ClaudePermissionMode, ContentBlock,
    DriverInput, InstanceSpec, PermissionMode, PromptInput, PromptMode, TextBlock,
};
use std::collections::BTreeMap;
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
    let deps_dir = std::env::current_exe().unwrap();
    let deps_dir = deps_dir.parent().unwrap();
    for candidate in [deps_dir.join("remuda"), deps_dir.join("../remuda")] {
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("no remuda relay next to {deps_dir:?}; build -p remuda or set REMUDA_NATIVE_LIVE_RELAY");
}

fn spec_for(cwd: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = DriverKind::ShellPty;
    spec.kind = AgentKind::Claude;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("haiku".into());
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: ClaudeInteractionMode::NativeTty,
    }));
    spec
}

fn pong_prompt() -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: "Reply with exactly the single word PONG and nothing else.".into(),
        }))],
        origin: remuda_protocol::InputOrigin::Human,
        native_client_message_id: "probe-1".into(),
    }))
}

async fn screen_text(driver: &ShellPtyDriver) -> String {
    let Some(bridge) = driver.tty_bridge().await else {
        return String::new();
    };
    let remuda_driver::tty::TtyBridge::Local(pty) = bridge else {
        return String::new();
    };
    let snap = pty.screen_snapshot();
    let raw = String::from_utf8_lossy(&snap.bytes).to_string();
    let lines = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take(120).collect::<String>())
        .collect::<Vec<_>>();
    let ring = String::from_utf8_lossy(&pty.snapshot()).replace('\x1b', "<ESC>");
    let ring_tail: String = ring
        .chars()
        .rev()
        .take(1500)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "source={:?}\n{}\n--- raw ring tail ---\n{ring_tail}",
        snap.source,
        lines.join("\n")
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live: real claude binary through the native shell-pty carrier"]
async fn real_claude_through_native_shell_pty() {
    let scenario =
        std::env::var("REMUDA_NATIVE_LIVE_SCENARIO").unwrap_or_else(|_| "baseline".into());
    let fixed = scenario == "fixed";
    eprintln!("scenario={scenario}");
    let owned_dir;
    let root: PathBuf = match std::env::var("REMUDA_NATIVE_LIVE_ROOT") {
        Ok(path) => {
            let base = PathBuf::from(path);
            std::fs::create_dir_all(&base).unwrap();
            let d = tempfile::tempdir_in(base).unwrap();
            let p = d.path().to_path_buf();
            // Manual repro dir: intentionally leak it so artifacts survive.
            std::mem::forget(d);
            owned_dir = None;
            p
        }
        Err(_) => {
            owned_dir = Some(tempfile::tempdir().unwrap());
            owned_dir.as_ref().unwrap().path().to_path_buf()
        }
    };
    let _ = &owned_dir;
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let instance_dir = root.join("instance");
    // The production default for real credentials is the operator's inherited
    // home; the scoped-home path is covered by the fake-harness node tests.
    let host_claude_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude"))
        .expect("HOME");
    let mut options = ShellPtyOptions::agent(
        workspace.clone(),
        AgentKind::Claude,
        AgentLaunch {
            profile: Box::new(profile()),
            launch_dir: instance_dir.join("launch"),
            native_home: host_claude_home.clone(),
            binary: Some(which_claude()),
            origin: LaunchOrigin::Human,
            settings_overlay: None,
        },
    );
    options.emulator = true;
    options.cols = 100;
    options.rows = 30;
    options.claude_home = Some(host_claude_home);
    if fixed {
        // The exact trust dialog is answered on screen, once, only for a
        // registered workspace — never by editing the operator's real config.
        options.auto_trust_workspace = true;
    }
    options.hooks = Some(HookConfig {
        instance_dir: instance_dir.clone(),
        relay_binary: relay_bin(),
        tui: remuda_driver::TuiMode::Default,
    });

    let driver = ShellPtyDriver::new(options);
    let spec = spec_for(&workspace);
    let mut events = driver.start(spec).await.expect("start").into_events();

    // Drain events in the background, bucket by channel.
    let channels = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::<String, u32>::new()));
    let names = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let channels = std::sync::Arc::clone(&channels);
        let names = std::sync::Arc::clone(&names);
        tokio::spawn(async move {
            while let Some(obs) = events.recv().await {
                *channels
                    .lock()
                    .unwrap()
                    .entry(format!("{:?}", obs.source.channel).to_lowercase())
                    .or_insert(0) += 1;
                let label = match &obs.body {
                    remuda_protocol::ObservationPayload::Lifecycle(l) => match l.as_ref() {
                        remuda_protocol::LifecyclePayload::Native(n) => {
                            format!("lifecycle:{}", n.native_name)
                        }
                        remuda_protocol::LifecyclePayload::Entity(_) => "lifecycle:entity".into(),
                    },
                    remuda_protocol::ObservationPayload::Message(_) => "message".into(),
                    other => format!("{other:?}"),
                };
                names.lock().unwrap().push(format!(
                    "{}:{label}",
                    format!("{:?}", obs.source.channel).to_lowercase()
                ));
            }
        });
    }

    // Poll readiness + dump screens; send the probe the moment it is ready.
    let start = Instant::now();
    let mut sent = false;
    let mut last_dump = String::new();
    while start.elapsed() < Duration::from_secs(40) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let screen = screen_text(&driver).await;
        let snap = format!("[{:?}]\n{screen}", start.elapsed());
        if snap != last_dump && start.elapsed().as_millis() % 3000 < 500 {
            eprintln!("{snap}\n");
            last_dump = snap;
        }
        if !sent && Driver::wait_control(&driver).await.is_ok() {
            eprintln!(">>> composer ready at {:?}; sending PONG", start.elapsed());
            driver.send(pong_prompt()).await.expect("send");
            sent = true;
        }
        if sent && start.elapsed() > Duration::from_secs(30) {
            break;
        }
    }

    eprintln!("sent={sent}");
    eprintln!("channels: {:#?}", channels.lock().unwrap());
    for name in names.lock().unwrap().iter() {
        eprintln!("event {name}");
    }
    eprintln!("final screen:\n{}", screen_text(&driver).await);

    Driver::close(&driver).await.unwrap();
}

fn which_claude() -> PathBuf {
    if let Ok(path) = std::env::var("REMUDA_NATIVE_LIVE_CLAUDE") {
        return PathBuf::from(path);
    }
    let path = std::env::var_os("PATH").unwrap();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("claude");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("no claude binary on PATH")
}
