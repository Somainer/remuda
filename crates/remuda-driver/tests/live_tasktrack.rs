//! Live c-tasktrack reproduction: real `claude` through ClaudePtyDriver,
//! exactly the Node's carrier (transcript pump + SessionStart hook watch).
//! Drives one foreground Agent subagent and one background Agent, then dumps
//! every relevant ToolCall / ToolResult / injected-user observation to JSONL
//! so the mapper/fold can be debugged against real records.

use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, DriverKind, LaunchOrigin,
    ProviderHealth, ProviderKind, ProviderProfile, RunHandle,
};
use remuda_herdr::{AgentReadParams, Client, ReadFormat, ReadSource, session_sockets};
use remuda_protocol::{
    ClaudeInteractionMode, ClaudePermission, ClaudePermissionMode, ContentBlock, DriverInput,
    InputOrigin, InstanceSpec, Knowledge, LifecyclePayload, Observation, ObservationPayload,
    PermissionMode, PromptInput, PromptMode, TextBlock,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
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

fn spec(cwd: &Path, driver: DriverKind) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = driver;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("haiku".into());
    spec.args = vec!["--max-budget-usd".into(), "0.5".into()];
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: ClaudeInteractionMode::NativeTty,
    }));
    spec
}

fn prompt_with(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "m1".into(),
    }))
}

fn isolated_native_home() -> PathBuf {
    let dest = PathBuf::from("/tmp/remuda-driver/tt-native-home");
    fs::create_dir_all(&dest).unwrap();
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let src = home.join(".claude.json");
    if src.is_file() {
        let dest_file = dest.join(".claude.json");
        fs::copy(&src, &dest_file).unwrap();
    }
    dest
}

fn cleanup(session: &str) {
    let _ = Command::new("herdr")
        .args(["session", "stop", session])
        .status();
    let _ = Command::new("herdr")
        .args(["session", "delete", session])
        .status();
}

async fn read_screen(client: &Client, target: &str) -> String {
    match client
        .agent_read(AgentReadParams {
            target: target.to_string(),
            source: ReadSource::RecentUnwrapped,
            lines: Some(80),
            format: ReadFormat::Text,
            strip_ansi: true,
        })
        .await
    {
        Ok(read) => read.text().to_string(),
        Err(_) => String::new(),
    }
}

async fn wait_screen(client: &Client, target: &str, needle: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if read_screen(client, target).await.contains(needle) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

fn is_idle(obs: &Observation) -> bool {
    matches!(&obs.body, ObservationPayload::Lifecycle(payload)
        if matches!(payload.as_ref(), LifecyclePayload::Native(native)
            if native.native_name == "agent_status"
                && matches!(&native.status, Knowledge::Known { value } if value == "idle")))
}

/// Collect observations until four consecutive seconds of `agent_status=idle`.
async fn collect_until_idle(
    handle: &mut RunHandle,
    seen: &mut Vec<Observation>,
    secs: u64,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut idle_since: Option<Instant> = None;
    while Instant::now() < deadline {
        if let Some(since) = idle_since
            && since.elapsed() > Duration::from_secs(4)
        {
            return true;
        }
        let next = tokio::time::timeout(Duration::from_millis(800), handle.recv()).await;
        if let Ok(Some(obs)) = next {
            if is_idle(&obs) {
                idle_since.get_or_insert_with(Instant::now);
            } else {
                idle_since = None;
            }
            seen.push(obs);
        }
    }
    false
}

#[ignore = "live: real claude pty spawning foreground + background Agent"]
#[tokio::test]
async fn live_tasktrack_fg_and_bg_agent() {
    let session = "remuda-tt-repro";
    // The test process itself is a Claude child session: CLAUDE_CODE_CHILD_SESSION
    // disables transcript persistence in the claude spawned through herdr,
    // blinding the transcript pump. The wrapper scrubs these (`env -u`);
    // unsafe env mutation is forbidden workspace-wide.
    cleanup(session);
    let root = PathBuf::from("/tmp/remuda-driver/tt-repro");
    let cwd = root.join("work");
    let launch = root.join("launch");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&launch).unwrap();
    let dump = root.join("observations.jsonl");
    let _ = fs::remove_file(&dump);

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: isolated_native_home(),
        binary: BinarySource::Command("claude".into()),
        origin: LaunchOrigin::Human,
        session_name: session.into(),
        socket_dir: None,
        herdr_binary: None,
        broker: std::sync::Arc::new(remuda_driver::EnvFileSecretBroker::env_only()),
        extra_env: Default::default(),
        agent_mcp: None,
        setting_sources: None,
        agent_start_timeout_ms: 180_000,
        inherit_default_config: false,
        settings_overlay_path: None,
        auto_trust_registered_workspace: false,
        seed_onboarding: true,
        host_claude_config: remuda_driver::HostClaudeConfig::from_env(),
    });

    let mut handle = driver
        .start(spec(&cwd, DriverKind::ClaudePty))
        .await
        .expect("pty start");
    let ack = handle.ack().clone();
    let agent = ack.native_ids.get("agentName").cloned().expect("agentName");
    let pane_id = ack.native_ids.get("paneId").cloned().expect("paneId");
    let client = Client::connect(session_sockets(session).api)
        .with_session_name(session)
        .with_timeout(Duration::from_secs(30));
    if wait_screen(&client, &agent, "Yes, I accept", 30).await {
        let _ = client
            .pane_send_keys(&pane_id, vec!["down".into(), "enter".into()])
            .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let mut seen: Vec<Observation> = Vec::new();
    let _ = collect_until_idle(&mut handle, &mut seen, 10).await;
    seen.clear();

    driver
        .send(prompt_with(
            "Do exactly this and nothing else: call the Agent tool once with \
             subagent_type \"general-purpose\", description \"foreground repro\", \
             and prompt \"Use the Bash tool to run: echo foreground-done ; then \
             reply with the single word DONE.\" Wait for it to finish, then tell \
             me its result in one line.",
        ))
        .await
        .expect("fg prompt");
    let fg_idle = collect_until_idle(&mut handle, &mut seen, 240).await;
    eprintln!("foreground reached idle: {fg_idle}");

    let mut bg_seen: Vec<Observation> = Vec::new();
    driver
        .send(prompt_with(
            "Do exactly this and nothing else: call the Agent tool once with \
             subagent_type \"general-purpose\", description \"background repro\", \
             run_in_background set to true, and prompt \"Use the Bash tool to run: \
             echo background-done ; then reply with the single word DONE.\" \
             After the tool returns, reply with exactly LAUNCHED and stop.",
        ))
        .await
        .expect("bg prompt");
    let bg_idle = collect_until_idle(&mut handle, &mut bg_seen, 240).await;
    eprintln!("background launch turn idle: {bg_idle}");

    // The completion notification is queued and delivered into the session
    // after the launch turn ends; keep reading observations for another while.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut notified = false;
    while Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(800), handle.recv()).await;
        let Ok(Some(obs)) = next else { continue };
        if matches!(&obs.body, ObservationPayload::Message(m)
            if serde_json::to_string(&m.blocks)
                .unwrap_or_default()
                .contains("task-notification"))
        {
            notified = true;
        }
        bg_seen.push(obs);
        if notified {
            tokio::time::sleep(Duration::from_secs(8)).await;
            break;
        }
    }
    eprintln!("background completion notification seen: {notified}");

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&dump)
        .unwrap();
    for obs in seen.iter().chain(bg_seen.iter()) {
        let line = serde_json::to_string(&obs.body).unwrap_or_default();
        if line.contains("Agent")
            || line.contains("task-notification")
            || line.contains("async_launched")
        {
            writeln!(file, "{line}").unwrap();
        }
    }
    drop(file);
    if let Some(path) = driver.session_transcript().await {
        eprintln!("transcript: {}", path.display());
    }
    eprintln!("dumped relevant observations to {}", dump.display());

    driver.close().await.expect("close");
    cleanup(session);
}
