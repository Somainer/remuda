//! `instance.send` over the native shell-pty carrier against the deterministic
//! `fake-harness`, in the demo's own configuration (native carrier, emulator
//! on, hooks on). No real model and no network: the harness plays a scripted
//! Claude turn in milliseconds.
//!
//! Two facts the ssh-stdio demo got wrong and this pins:
//! * a send while the harness is **idle** lands in the composer and submits,
//!   and the scripted turn starts;
//! * a send while a turn is **running** is not written into that turn — the
//!   driver refuses control mid-turn (`ControlUnavailable`), the way the Node's
//!   D-022 delivery tick holds the prompt — and is delivered once the turn ends.
//!
//! The driver is built exactly the way `shell_pty_native_live.rs` builds one,
//! only pointed at the fake harness binary.
#![cfg(unix)]

use remuda_driver::shell_pty::{AgentLaunch, HookConfig, ShellPtyDriver, ShellPtyOptions};
use remuda_driver::{
    Delegation, Driver, DriverKind, LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_protocol::{
    AgentKind, ContentBlock, DriverInput, InstanceSpec, PromptInput, PromptMode, TextBlock,
};
use remuda_screen::ScreenStatus;
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

/// The `remuda` CLI the hook overlay invokes as its relay, next to this test
/// binary (built by the same `cargo test` invocation).
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
    // Fall back to the debug binary the workspace builds.
    remuda_testing::workspace_root().join("target/debug/remuda")
}

fn spec_for(cwd: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = DriverKind::ShellPty;
    spec.kind = AgentKind::Claude;
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
        origin: remuda_protocol::InputOrigin::Human,
        native_client_message_id: "probe".into(),
    }))
}

/// A scenario with one slow tool so the turn stays in the working state long
/// enough to prove the driver refuses control mid-turn, then returns to idle.
const SCENARIO: &str = r#"{
  "turns": [
    {
      "match_prefix": "PROBE_ONE",
      "text": "done one",
      "tools": [
        { "name": "Bash", "input": { "command": "sleep" }, "approval": "auto", "duration_ms": 2500 }
      ]
    },
    {
      "match_prefix": "PROBE_TWO",
      "text": "done two"
    }
  ]
}"#;

async fn screen_status(driver: &ShellPtyDriver) -> Option<ScreenStatus> {
    driver.screen_status()
}

/// Poll `driver` for a screen status matching `want` within `budget`.
async fn await_status(driver: &ShellPtyDriver, want: ScreenStatus, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if screen_status(driver).await == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn install_fake_claude(root: &Path) -> PathBuf {
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).unwrap();
    let source = ensure_workspace_bin("fake-harness");
    let dest = install.join("claude");
    std::fs::copy(&source, &dest).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    dest
}

fn driver_for(root: &Path, scenario_path: &Path) -> ShellPtyDriver {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let native_home = root.join("home");
    std::fs::create_dir_all(&native_home).unwrap();
    let instance_dir = root.join("instance");
    let binary = install_fake_claude(root);

    let mut options = ShellPtyOptions::agent(
        workspace.clone(),
        AgentKind::Claude,
        AgentLaunch {
            profile: Box::new(profile()),
            launch_dir: instance_dir.join("launch"),
            native_home: native_home.clone(),
            binary: Some(binary),
            origin: LaunchOrigin::Human,
            settings_overlay: None,
        },
    );
    options.emulator = true;
    options.cols = 100;
    options.rows = 30;
    options.claude_home = Some(native_home);
    // Match the demo: the fake harness plays claude 2.1.270 and fires its hooks
    // through the settings overlay the recipe writes.
    options
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());
    options.extra_env.insert(
        "FAKE_HARNESS_SCRIPT".into(),
        scenario_path.to_string_lossy().into_owned(),
    );
    options.hooks = Some(HookConfig {
        instance_dir,
        relay_binary: relay_bin(),
        tui: remuda_driver::TuiMode::Default,
    });
    ShellPtyDriver::new(options)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_lands_when_idle_and_is_held_until_a_running_turn_ends() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let scenario_path = root.join("scenario.json");
    std::fs::write(&scenario_path, SCENARIO).unwrap();

    let driver = driver_for(root, &scenario_path);
    let spec = spec_for(&root.join("workspace"));
    let _events = driver.start(spec).await.expect("start").into_events();

    // The composer must become ready before the first send.
    let ready = {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut ok = false;
        while Instant::now() < deadline {
            if Driver::wait_control(&driver).await.is_ok() {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        ok
    };
    assert!(ready, "composer never became ready for the first send");

    // (1) Idle send: it lands in the composer, submits, and the scripted turn
    // starts — the slow tool drives the screen to Working.
    driver.send(prompt("PROBE_ONE")).await.expect("idle send");
    assert!(
        await_status(&driver, ScreenStatus::Working, Duration::from_secs(10)).await,
        "the idle send never started a turn: status={:?}",
        driver.screen_status()
    );

    // (2) Mid-turn: the driver must refuse control rather than typing into the
    // running turn — the same refusal the Node's D-022 tick reads to hold the
    // prompt for the next turn.
    let mid_turn = Driver::wait_control(&driver).await;
    assert!(
        matches!(
            mid_turn,
            Err(remuda_driver::DriverError::ControlUnavailable)
        ),
        "control must be refused mid-turn, got {mid_turn:?}"
    );

    // The turn ends: the screen returns to idle and control is available again.
    assert!(
        await_status(&driver, ScreenStatus::Idle, Duration::from_secs(15)).await,
        "the turn never returned to idle: status={:?}",
        driver.screen_status()
    );
    let after = {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ok = false;
        while Instant::now() < deadline {
            if Driver::wait_control(&driver).await.is_ok() {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        ok
    };
    assert!(after, "control never returned after the turn ended");

    // (3) The held send is now delivered and starts its own turn.
    driver
        .send(prompt("PROBE_TWO"))
        .await
        .expect("post-turn send");
    assert!(
        await_status(&driver, ScreenStatus::Working, Duration::from_secs(10)).await
            || await_status(&driver, ScreenStatus::Idle, Duration::from_secs(2)).await,
        "the post-turn send never reached the harness: status={:?}",
        driver.screen_status()
    );

    Driver::close(&driver).await.unwrap();
}
