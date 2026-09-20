//! Review tests for `claude_pty.rs` / `claude_bg.rs` (recipe resume, hooks, attach).
//! Does not share helpers with `claude_pty.rs`, `claude_bg.rs`, or `live_claude.rs`.

use remuda_driver::claude_bg::review as bg_review;
use remuda_driver::claude_pty::{ClaudePtyDriver, ClaudePtyOptions, review};
use remuda_driver::{
    BinarySource, ClaudeBgDriver, ClaudeBgOptions, Delegation, Driver, DriverError, DriverKind,
    LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile, parse_backgrounded, pin_binary,
};
use remuda_protocol::{
    AgentKind, ClaudeBgRef, ClaudePermission, ClaudePermissionMode, ClaudeRef, Completeness,
    ContentBlock, DriverInput, InputOrigin, InstanceSpec, Knowledge, NativeRef, ObservationPayload,
    PermissionMode, PromptInput, PromptMode, TextBlock,
};
use remuda_testing::{
    FakeHerdrOptions, FakeHerdrServer, ShortTempDir, ensure_workspace_bin, install_executable,
};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn ensure_fake_herdr_bin() -> PathBuf {
    ensure_workspace_bin("fake-herdr")
}

fn stub_claude(dir: &Path) -> PathBuf {
    install_executable(dir, "claude", "#!/bin/sh\necho '2.1.268 (Claude Code)'\n")
}

const BG_STUB: &str = r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
mkdir -p "$home"
log="$home/stub-argv.log"
printf '%s\n' "$*" >> "$log"
if [ "$1" = "--version" ]; then
  echo "2.1.268 (Claude Code)"
  exit 0
fi
bg=0
name="stub-bg"
stop=0
agents=0
json=0
id="abcdef12"
session="abcdef12-0000-4000-8000-000000000001"
while [ $# -gt 0 ]; do
  case "$1" in
    --bg) bg=1 ;;
    --name) shift; name="$1" ;;
    --resume) shift ;;
    --session-id) shift ;;
    --cwd) shift; if [ "$bg" -eq 1 ]; then echo "unknown option --cwd" >&2; exit 1; fi ;;
    agents) agents=1 ;;
    --json) json=1 ;;
    stop) stop=1; shift; id="$1" ;;
    attach) echo "attach would wake"; exit 0 ;;
    rm) echo "rm is forbidden in remuda review stub" >&2; exit 2 ;;
  esac
  shift
done
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

fn stub_bg_claude(dir: &Path) -> PathBuf {
    install_executable(dir, "claude", BG_STUB)
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

fn load_spec() -> InstanceSpec {
    serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap()
}

fn pty_spec(cwd: &Path, model: &str) -> InstanceSpec {
    let mut spec = load_spec();
    spec.driver = DriverKind::ClaudePty;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some(model.into());
    spec
}

fn bg_spec(cwd: &Path, model: &str) -> InstanceSpec {
    let mut spec = load_spec();
    spec.driver = DriverKind::ClaudeBg;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some(model.into());
    spec
}

fn prompt(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "m-review".into(),
    }))
}

fn has_flag_value(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == flag && pair[1] == value)
}

fn has_token(argv: &[String], token: &str) -> bool {
    argv.iter().any(|item| item == token)
}

fn native_ref_bg(spec: &InstanceSpec, job_id: &str, session: &str) -> NativeRef {
    NativeRef {
        host_id: spec.host.clone(),
        native_store_id: spec.native_home.store_id.clone(),
        kind: AgentKind::Claude,
        session_id: Knowledge::Known {
            value: session.into(),
        },
        transcript: Knowledge::Unknown {
            reason: "review".into(),
            evidence_event_ids: vec![],
        },
        signal_tier: None,
        capabilities: Vec::new(),
        codex: None,
        acp: None,
        claude: Some(ClaudeRef {
            session_id: session.into(),
        }),
        claude_bg: Some(ClaudeBgRef {
            job_id: job_id.into(),
        }),
        agy: None,
        herdr: None,
    }
}

#[test]
fn resume_argv_keeps_settings_and_model() {
    let argv = vec![
        "--dangerously-skip-permissions".into(),
        "--setting-sources".into(),
        "user,project,local".into(),
        "--settings".into(),
        "/tmp/launch/settings.json".into(),
        "--model".into(),
        "opus-review".into(),
        "--session-id".into(),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
    ];
    let resumed = review::resume_argv(&argv, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
    assert!(has_flag_value(
        &resumed,
        "--resume",
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
    ));
    assert!(has_flag_value(&resumed, "--model", "opus-review"));
    assert!(has_flag_value(
        &resumed,
        "--settings",
        "/tmp/launch/settings.json"
    ));
    assert!(has_flag_value(
        &resumed,
        "--setting-sources",
        "user,project,local"
    ));
    assert!(has_token(&resumed, "--dangerously-skip-permissions"));
    assert!(!has_token(&resumed, "--session-id"));
    assert!(!has_token(&resumed, "--continue"));
    assert!(!has_token(&resumed, "--bare"));
}

#[test]
fn refuse_bare_rejects_equals_form() {
    assert!(review::refuse_argv(&["--model".into(), "haiku".into()]).is_ok());
    assert!(review::refuse_argv(&["--bare".into()]).is_err());
    assert!(review::refuse_argv(&["--bare=1".into()]).is_err());
    assert!(review::refuse_argv(&["--no-session-persistence".into()]).is_err());
    assert!(review::refuse_argv(&["--continue".into()]).is_err());
}

#[test]
fn session_start_merge_appends_and_keeps_other_events() {
    let mut settings = json!({
        "model": "keep-me",
        "hooks": {
            "PreToolUse": [{
                "hooks": [{"type": "command", "command": "user-pre"}]
            }],
            "SessionStart": [{
                "hooks": [{"type": "command", "command": "user-global"}]
            }]
        }
    });
    review::merge_hooks(&mut settings, "remuda-session-start").unwrap();
    review::merge_hooks(&mut settings, "remuda-session-start").unwrap();
    assert_eq!(settings["model"], "keep-me");
    let pre = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert_eq!(pre, "user-pre");
    let start = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(start.len(), 2, "existing SessionStart matcher must stay");
    let commands: Vec<_> = start
        .iter()
        .filter_map(|matcher| matcher["hooks"][0]["command"].as_str())
        .collect();
    assert_eq!(commands, ["user-global", "remuda-session-start"]);
}

#[test]
fn inject_hook_writes_launch_dir_not_native_home_and_keeps_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&launch).unwrap();
    fs::create_dir_all(&home).unwrap();
    let mut recipe = remuda_driver::materialize(&remuda_driver::MaterializeRequest {
        spec: &{
            let cwd = tmp.path().join("work");
            fs::create_dir_all(&cwd).unwrap();
            pty_spec(&cwd, "opus-review")
        },
        profile: &profile(),
        launch_dir: launch.clone(),
        native_home: home.clone(),
        session: remuda_driver::SessionAction::New {
            session_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        },
        launch_id: remuda_protocol::Id::new("launch").unwrap(),
        binary: BinarySource::Pinned(pin_binary(stub_claude(tmp.path())).unwrap()),
        setting_sources: None,
        origin: LaunchOrigin::Human,
        native_home_managed: None,
        settings_overlay_path: None,
        secret_policy: None,
    })
    .unwrap();
    let injected = review::inject_hook(&mut recipe, &launch).unwrap();
    assert!(launch.join("session-start.sh").is_file());
    assert!(launch.join("settings.json").is_file());
    assert!(
        !home.join("settings.json").exists(),
        "must not clobber native home settings"
    );
    assert!(
        !home.join("hooks").exists(),
        "must not write user global hooks"
    );
    assert!(has_token(&injected.argv, "--settings"));
    assert!(!has_token(&injected.argv, "--setting-sources"));
    let overlay: serde_json::Value =
        serde_json::from_slice(&fs::read(launch.join("settings.json")).unwrap()).unwrap();
    assert!(
        overlay["hooks"]["SessionStart"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );
}

#[test]
fn blocked_interaction_is_screen_derived_and_not_answerable() {
    let payloads = review::blocked_status_payloads().unwrap();
    let interaction = payloads.iter().find_map(|(completeness, payload)| {
        if let ObservationPayload::InteractionRequested(body) = payload {
            Some((*completeness, body.interaction.answerable))
        } else {
            None
        }
    });
    let (completeness, answerable) = interaction.expect("blocked must emit interaction");
    assert_eq!(completeness, Completeness::ScreenDerived);
    assert!(!answerable);
    assert!(
        payloads.iter().any(|(completeness, payload)| {
            *completeness == Completeness::ScreenDerived
                && matches!(payload, ObservationPayload::Lifecycle(_))
        }),
        "blocked lifecycle must also be screen-derived"
    );
}

#[test]
fn tty_bypass_strips_print_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let mut spec = pty_spec(&cwd, "opus-review");
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: remuda_protocol::ClaudeInteractionMode::NativeTty,
    }));
    let mut recipe = remuda_driver::materialize(&remuda_driver::MaterializeRequest {
        spec: &spec,
        profile: &profile(),
        launch_dir: launch,
        native_home: home,
        session: remuda_driver::SessionAction::New {
            session_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        },
        launch_id: remuda_protocol::Id::new("launch").unwrap(),
        binary: BinarySource::Pinned(pin_binary(stub_claude(tmp.path())).unwrap()),
        setting_sources: None,
        origin: LaunchOrigin::Human,
        native_home_managed: None,
        settings_overlay_path: None,
        secret_policy: None,
    })
    .unwrap();
    recipe
        .argv
        .push("--allow-dangerously-skip-permissions".into());
    review::apply_bypass(&mut recipe);
    assert!(has_token(&recipe.argv, "--dangerously-skip-permissions"));
    assert!(!has_token(
        &recipe.argv,
        "--allow-dangerously-skip-permissions"
    ));
}

#[test]
fn parse_backgrounded_short_id_and_uuid() {
    assert_eq!(
        parse_backgrounded("backgrounded · b944e31c · hh-probe-ctl-bg\n  claude agents\n")
            .as_deref(),
        Some("b944e31c")
    );
    assert_eq!(
        parse_backgrounded("backgrounded · b944e31c-bd52-4781-af13-60f8da462b67 · name").as_deref(),
        Some("b944e31c")
    );
    assert_eq!(
        parse_backgrounded("backgrounded·abcdef12·name").as_deref(),
        Some("abcdef12")
    );
    assert!(parse_backgrounded("claude attach b944e31c").is_none());
    assert!(parse_backgrounded("not a bg line").is_none());
}

#[test]
fn stop_invocation_is_stop_not_rm() {
    let args = bg_review::stop_invocation("b944e31c");
    assert_eq!(args, ["stop", "b944e31c"]);
    assert!(!args.iter().any(|token| token == "rm"));
}

#[test]
fn bot_origin_rejects_bypass_and_dont_ask() {
    let mut spec = load_spec();
    spec.driver = DriverKind::ClaudePty;
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: remuda_protocol::ClaudeInteractionMode::NativeTty,
    }));
    assert!(matches!(
        review::reject_bot(&spec),
        Err(DriverError::BypassNotAllowedForBot)
    ));
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::DontAsk,
        interaction: remuda_protocol::ClaudeInteractionMode::NativeTty,
    }));
    assert!(matches!(
        review::reject_bot(&spec),
        Err(DriverError::BypassNotAllowedForBot)
    ));
}

#[test]
fn ensure_sources_rejects_empty_and_leaves_normal_sources_implicit() {
    let argv = vec!["--model".into(), "haiku".into()];
    review::ensure_sources(&argv).unwrap();
    assert!(!has_token(&argv, "--setting-sources"));
    let empty = vec!["--setting-sources".into(), "".into()];
    assert!(review::ensure_sources(&empty).is_err());
}

#[tokio::test]
async fn pty_resume_keeps_settings_model_and_never_bare() {
    let tmp = tempfile::tempdir().unwrap();
    let socket_root = ShortTempDir::new().unwrap();
    let socket_dir = socket_root.path().join("herdr");
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

    let mut spec = pty_spec(&cwd, "opus-review");
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: remuda_protocol::ClaudeInteractionMode::NativeTty,
    }));
    spec.args = vec!["--max-budget-usd".into(), "0.3".into()];
    let handle = driver.start(spec.clone()).await.expect("start");
    let first = handle.recipe().argv.clone();
    assert!(has_flag_value(&first, "--model", "opus-review"));
    assert!(has_token(&first, "--settings"));
    assert!(!has_token(&first, "--setting-sources"));
    assert!(has_token(&first, "--dangerously-skip-permissions"));
    assert!(!has_token(&first, "--allow-dangerously-skip-permissions"));
    assert!(!has_token(&first, "--bare"));
    let session = handle
        .ack()
        .native_ids
        .get("sessionId")
        .cloned()
        .expect("session");
    driver.close().await.expect("close");

    let native = NativeRef {
        host_id: spec.host.clone(),
        native_store_id: spec.native_home.store_id.clone(),
        kind: AgentKind::Claude,
        session_id: Knowledge::Known {
            value: session.clone(),
        },
        transcript: Knowledge::Unknown {
            reason: "review".into(),
            evidence_event_ids: vec![],
        },
        signal_tier: None,
        capabilities: Vec::new(),
        codex: None,
        acp: None,
        claude: Some(ClaudeRef {
            session_id: session.clone(),
        }),
        claude_bg: None,
        agy: None,
        herdr: None,
    };
    let resumed = driver.resume(native).await.expect("resume from last_spec");
    let argv = &resumed.recipe().argv;
    assert!(has_flag_value(argv, "--resume", &session));
    assert!(has_flag_value(argv, "--model", "opus-review"));
    assert!(has_token(argv, "--settings"));
    assert!(!has_token(argv, "--setting-sources"));
    assert!(has_token(argv, "--dangerously-skip-permissions"));
    assert!(has_flag_value(argv, "--max-budget-usd", "0.3"));
    assert!(!has_token(argv, "--bare"));
    assert!(!has_token(argv, "--continue"));
    driver.close().await.expect("close resume");
}

#[tokio::test]
async fn bg_resume_keeps_settings_attach_after_stop_does_not_wake() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let claude = stub_bg_claude(tmp.path());
    let driver = ClaudeBgDriver::new(ClaudeBgOptions {
        instance_id: None,
        profile: profile(),
        launch_dir: launch.clone(),
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

    let spec = bg_spec(&cwd, "opus-review");
    let handle = driver.start(spec.clone()).await.expect("prepare");
    let prepared = handle.recipe().argv.clone();
    assert!(has_token(&prepared, "--bg"));
    assert!(!has_token(&prepared, "--session-id"));
    assert!(!has_token(&prepared, "--cwd"));
    assert!(!has_token(&prepared, "--bare"));
    assert!(has_token(&prepared, "--settings"));
    assert!(has_flag_value(&prepared, "--model", "opus-review"));
    assert!(!has_token(&prepared, "--setting-sources"));
    assert!(launch.join("session-start.sh").is_file());

    let ack = driver
        .send(prompt("Reply with exactly OK"))
        .await
        .expect("dispatch");
    let job_id = ack.native_ids.get("jobId").cloned().expect("jobId");
    assert_eq!(job_id, "abcdef12");

    let live_attach = driver
        .attach(native_ref_bg(
            &spec,
            &job_id,
            "abcdef12-0000-4000-8000-000000000001",
        ))
        .await
        .expect("live attach is observe");
    assert_eq!(
        live_attach.native_ids.get("jobId").map(String::as_str),
        Some("abcdef12")
    );

    driver.close().await.expect("stop");
    let log_body = fs::read_to_string(home.join("stub-argv.log")).unwrap_or_default();
    assert!(
        log_body
            .lines()
            .any(|line| line.split_whitespace().any(|t| t == "stop")),
        "close must invoke claude stop: {log_body}"
    );
    assert!(
        !log_body
            .lines()
            .any(|line| line.split_whitespace().any(|t| t == "rm")),
        "close must never rm: {log_body}"
    );

    let stopped = driver
        .attach(native_ref_bg(
            &spec,
            &job_id,
            "abcdef12-0000-4000-8000-000000000001",
        ))
        .await
        .expect_err("attach after stop must not wake");
    assert!(matches!(stopped, DriverError::AttachWouldWake));

    let resumed = driver
        .resume(native_ref_bg(
            &spec,
            &job_id,
            "abcdef12-0000-4000-8000-000000000001",
        ))
        .await
        .expect("resume of stopped-not-rm must restore recipe");
    let argv = &resumed.recipe().argv;
    assert!(has_flag_value(argv, "--model", "opus-review"));
    assert!(has_token(argv, "--settings"));
    assert!(has_token(argv, "--bg"));
    assert!(!has_token(argv, "--session-id"));
    assert!(!has_token(argv, "--bare"));

    let still_stopped = driver
        .attach(native_ref_bg(
            &spec,
            &job_id,
            "abcdef12-0000-4000-8000-000000000001",
        ))
        .await
        .expect_err("attach still must not wake a stopped job");
    assert!(matches!(still_stopped, DriverError::AttachWouldWake));
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn bg_attach_before_dispatch_would_wake() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let claude = stub_bg_claude(tmp.path());
    let driver = ClaudeBgDriver::new(ClaudeBgOptions {
        instance_id: None,
        profile: profile(),
        launch_dir: launch,
        native_home: home,
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
    let spec = bg_spec(&cwd, "opus-review");
    let _handle = driver.start(spec.clone()).await.expect("prepare");
    let err = driver
        .attach(native_ref_bg(
            &spec,
            "abcdef12",
            "abcdef12-0000-4000-8000-000000000001",
        ))
        .await
        .expect_err("undispatched attach");
    assert!(matches!(err, DriverError::AttachWouldWake));
    driver.close().await.expect("close undispatched");
}
