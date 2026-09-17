//! Offline `claude-bg` parse/stub flow plus one ignored live run.
//!
//! The stub binary stands in for `claude --bg` / `agents --json` / `stop`.

use remuda_driver::{
    BinarySource, ClaudeBgDriver, ClaudeBgOptions, Delegation, Driver, DriverKind, LaunchOrigin,
    ProviderHealth, ProviderKind, ProviderProfile, parse_backgrounded, pin_binary,
};
use remuda_protocol::{
    ClaudeBgRef, ContentBlock, DriverInput, InputOrigin, InstanceSpec, Knowledge, NativeRef,
    PromptInput, PromptMode, TextBlock,
};
use remuda_testing::install_executable;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const STUB: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "2.1.268 (Claude Code)"
  exit 0
fi
bg=0
name="stub-bg"
resume=""
stop=0
agents=0
json=0
cwd=""
id="abcdef12"
session="abcdef12-0000-4000-8000-000000000001"
while [ $# -gt 0 ]; do
  case "$1" in
    --bg) bg=1 ;;
    --name) shift; name="$1" ;;
    --resume) shift; resume="$1" ;;
    --session-id) shift ;;
    --cwd)
      shift
      cwd="$1"
      if [ "$bg" -eq 1 ]; then
        echo "unknown option --cwd" >&2
        exit 1
      fi
      ;;
    agents) agents=1 ;;
    --json) json=1 ;;
    stop) stop=1; shift; id="$1" ;;
    attach) echo "attach would wake"; exit 0 ;;
    rm) echo "rm is forbidden in remuda stub" >&2; exit 2 ;;
  esac
  shift
done
home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
mkdir -p "$home/jobs/$id"
if [ "$agents" -eq 1 ] && [ "$json" -eq 1 ]; then
  printf '[{"id":"%s","name":"%s","sessionId":"%s","status":"idle","state":"idle","kind":"background"}]\n' "$id" "$name" "$session"
  exit 0
fi
if [ "$stop" -eq 1 ]; then
  printf '{"state":"stopped","sessionId":"%s"}\n' "$session" > "$home/jobs/$id/state.json"
  exit 0
fi
if [ "$bg" -eq 1 ]; then
  printf '{"state":"idle","sessionId":"%s"}\n' "$session" > "$home/jobs/$id/state.json"
  printf 'backgrounded · %s · %s\n' "$id" "$name"
  exit 0
fi
echo "unhandled stub invocation" >&2
exit 1
"#;

fn stub_claude(dir: &Path) -> PathBuf {
    install_executable(dir, "claude", STUB)
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
    spec.driver = DriverKind::ClaudeBg;
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

#[test]
fn parse_backgrounded_line() {
    let stdout = "backgrounded · b944e31c · hh-probe-ctl-bg\n  claude agents\n";
    assert_eq!(parse_backgrounded(stdout).as_deref(), Some("b944e31c"));
    assert!(parse_backgrounded("not a bg line").is_none());
}

#[tokio::test]
async fn stub_bg_start_send_stop_does_not_rm() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let claude = stub_claude(tmp.path());

    let driver = ClaudeBgDriver::new(ClaudeBgOptions {
        instance_id: None,
        profile: profile(),
        launch_dir: launch,
        native_home: home.clone(),
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-test".into(),
        socket_dir: None,
        herdr_binary: None,
        broker: std::sync::Arc::new(remuda_driver::EnvFileSecretBroker::env_only()),
        extra_env: Default::default(),
        agent_mcp: None,
        setting_sources: None,
        inherit_default_config: false,
        settings_overlay_path: None,
    });

    let handle = driver.start(spec(&cwd)).await.expect("prepare");
    assert!(
        !handle
            .recipe()
            .argv
            .iter()
            .any(|token| token == "--session-id"),
        "bg must ignore --session-id: {:?}",
        handle.recipe().argv
    );
    assert!(handle.recipe().argv.iter().any(|token| token == "--bg"));
    assert!(!handle.recipe().argv.iter().any(|token| token == "--bare"));

    let ack = driver
        .send(prompt("Reply with exactly OK"))
        .await
        .expect("first send dispatches --bg");
    assert_eq!(
        ack.native_ids.get("jobId").map(String::as_str),
        Some("abcdef12")
    );
    assert_eq!(
        ack.native_ids.get("sessionId").map(String::as_str),
        Some("abcdef12-0000-4000-8000-000000000001")
    );

    let attach = driver
        .attach(NativeRef {
            host_id: spec(&cwd).host,
            native_store_id: spec(&cwd).native_home.store_id,
            kind: remuda_protocol::AgentKind::Claude,
            session_id: Knowledge::Known {
                value: "abcdef12-0000-4000-8000-000000000001".into(),
            },
            transcript: Knowledge::Unknown {
                reason: "test".into(),
                evidence_event_ids: vec![],
            },
            signal_tier: None,
            capabilities: Vec::new(),
            codex: None,
            acp: None,
            claude: None,
            claude_bg: Some(ClaudeBgRef {
                job_id: "abcdef12".into(),
            }),
            agy: None,
            herdr: None,
        })
        .await
        .expect("read-only attach must not wake");
    assert_eq!(
        attach.native_ids.get("jobId").map(String::as_str),
        Some("abcdef12")
    );

    driver.close().await.expect("stop");
    let state = fs::read_to_string(home.join("jobs/abcdef12/state.json")).unwrap();
    assert!(state.contains("stopped"));
    // Wait a tick so the observer task is aborted.
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[ignore = "live: claude --bg --model haiku once, then claude stop (never rm)"]
#[tokio::test]
async fn live_claude_bg_haiku_once() {
    let which = Command::new("which").arg("claude").status().unwrap();
    if !which.success() {
        eprintln!("skip: claude not on PATH");
        return;
    }
    let root = PathBuf::from("/tmp/remuda-driver");
    let cwd = root.join("bg-live");
    fs::create_dir_all(&cwd).unwrap();
    let launch = root.join("bg-launch");
    let home = root.join("bg-home");
    fs::create_dir_all(&home).unwrap();

    let driver = ClaudeBgDriver::new(ClaudeBgOptions {
        instance_id: None,
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
        inherit_default_config: false,
        settings_overlay_path: None,
    });
    let mut spec = spec(&cwd);
    spec.model_id = Some("haiku".into());
    spec.args = vec!["--max-budget-usd".into(), "0.3".into()];
    let _handle = driver.start(spec).await.expect("live prepare");
    driver
        .send(prompt(
            "Reply with exactly the word OK. Do not use any tools.",
        ))
        .await
        .expect("live --bg");
    driver.close().await.expect("live stop");
}
