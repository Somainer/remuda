//! Live haiku validation for claude-pty and claude-bg.
//!
//! Isolation: `/tmp/remuda-driver/` cwd + launch overlay. Native login uses
//! `$HOME/.claude`. Herdr session `remuda-test` only (never the default socket).

use remuda_driver::{
    BinarySource, ClaudeBgDriver, ClaudeBgOptions, ClaudePtyDriver, ClaudePtyOptions, Delegation,
    Driver, DriverKind, LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_herdr::{AgentReadParams, Client, ReadFormat, ReadSource, session_sockets};
use remuda_protocol::{
    ClaudeInteractionMode, ClaudePermission, ClaudePermissionMode, ContentBlock, DriverInput,
    InputOrigin, InstanceSpec, Knowledge, LifecyclePayload, ObservationPayload, PermissionMode,
    PromptInput, PromptMode, TextBlock,
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
    spec.args = vec!["--max-budget-usd".into(), "0.3".into()];
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: ClaudeInteractionMode::NativeTty,
    }));
    spec
}

fn prompt() -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: "Reply with exactly OK".into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "m1".into(),
    }))
}

fn isolated_native_home() -> PathBuf {
    let dest = PathBuf::from("/tmp/remuda-driver/native-home");
    fs::create_dir_all(&dest).unwrap();
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    // Default Claude oauth lives at ~/.claude.json. Setting CLAUDE_CONFIG_DIR to
    // ~/.claude makes the CLI look for ~/.claude/.claude.json and skip login.
    let src = home.join(".claude.json");
    if src.is_file() {
        let dest_file = dest.join(".claude.json");
        fs::copy(&src, &dest_file).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest_file, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    dest
}

fn evidence_path() -> PathBuf {
    let dir = PathBuf::from("/tmp/remuda-driver");
    let _ = fs::create_dir_all(&dir);
    dir.join("live-evidence.md")
}

fn append_evidence(line: &str) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(evidence_path())
        .expect("evidence file");
    writeln!(file, "{line}").expect("write evidence");
}

fn redact_path(path: impl AsRef<Path>) -> String {
    let path = path.as_ref().to_string_lossy();
    if let Ok(home) = std::env::var("HOME") {
        path.replace(&home, "$HOME")
    } else {
        path.into_owned()
    }
}

fn jsonl_type_histogram(path: &Path) -> String {
    let Ok(body) = fs::read_to_string(path) else {
        return "missing".into();
    };
    let mut counts = std::collections::BTreeMap::<String, u32>::new();
    let mut lines = 0u32;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        let kind = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".into());
        *counts.entry(kind).or_default() += 1;
    }
    format!("lines={lines} types={counts:?}")
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

async fn wait_screen_contains(client: &Client, target: &str, needle: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if read_screen(client, target).await.contains(needle) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    false
}

fn cleanup_herdr_session() {
    let _ = Command::new("herdr")
        .args(["session", "stop", "remuda-test"])
        .status();
    let _ = Command::new("herdr")
        .args(["session", "delete", "remuda-test"])
        .status();
}

#[ignore = "live: isolated remuda-test herdr + claude --model haiku once"]
#[tokio::test]
async fn live_claude_pty_start_prompt_idle_read_close() {
    let root = PathBuf::from("/tmp/remuda-driver");
    let cwd = root.join("pty-live");
    let launch = root.join("pty-launch");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&launch).unwrap();
    fs::write(evidence_path(), "").unwrap();

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: isolated_native_home(),
        binary: BinarySource::Command("claude".into()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-test".into(),
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
    });

    let t0 = Instant::now();
    append_evidence(&format!(
        "### pty\n\n- start: `ClaudePtyDriver` herdr session `remuda-test`, cwd `{}`, model haiku, `--max-budget-usd 0.3`, bypass",
        cwd.display()
    ));
    let mut handle = driver
        .start(spec(&cwd, DriverKind::ClaudePty))
        .await
        .expect("pty start");
    let start_ms = t0.elapsed().as_millis();
    let ack = handle.ack().clone();
    append_evidence(&format!(
        "- start ack in {start_ms}ms dispatch={:?} ids={:?}",
        ack.dispatch, ack.native_ids
    ));
    assert!(
        !handle.recipe().argv.iter().any(|t| t == "--bare"),
        "live argv contained --bare"
    );

    let mut events = Vec::new();
    if let Some(obs) = tokio::time::timeout(Duration::from_secs(3), handle.recv())
        .await
        .ok()
        .flatten()
    {
        events.push(format!("{:?}", obs.body.kind()));
    }

    let agent = ack.native_ids.get("agentName").cloned().expect("agentName");
    let pane_id = ack.native_ids.get("paneId").cloned().expect("paneId");
    let client = Client::connect(session_sockets("remuda-test").api)
        .with_session_name("remuda-test")
        .with_timeout(Duration::from_secs(30));
    if wait_screen_contains(&client, &agent, "Yes, I accept", 30).await {
        let _ = client
            .pane_send_keys(&pane_id, vec!["down".into(), "enter".into()])
            .await;
        append_evidence("- bypass warning visible; pane_send_keys down+enter");
        let dismissed = !wait_screen_contains(&client, &agent, "Yes, I accept", 2).await
            || !read_screen(&client, &agent).await.contains("Yes, I accept");
        append_evidence(&format!("- bypass warning dismissed={dismissed}"));
        tokio::time::sleep(Duration::from_secs(2)).await;
    } else {
        append_evidence("- no bypass warning on screen within 30s");
    }

    let t_prompt = Instant::now();
    driver.send(prompt()).await.expect("pty prompt");
    append_evidence(&format!(
        "- prompt `Reply with exactly OK` in {}ms",
        t_prompt.elapsed().as_millis()
    ));

    let mut idle = false;
    let mut hook = false;
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(800), handle.recv()).await;
        let Ok(Some(obs)) = next else {
            if !hook && let Some(path) = driver.session_transcript().await {
                hook = true;
                append_evidence(&format!(
                    "- SessionStart transcript `{}`",
                    redact_path(&path)
                ));
            }
            continue;
        };
        events.push(format!("{:?}", obs.body.kind()));
        if let ObservationPayload::Lifecycle(payload) = &obs.body
            && let LifecyclePayload::Native(native) = payload.as_ref()
        {
            if native.native_name == "SessionStart" {
                hook = true;
                if let Some(path) = native.related_ids.get("transcriptPath") {
                    append_evidence(&format!(
                        "- SessionStart event transcript `{}`",
                        redact_path(path)
                    ));
                }
            }
            if native.native_name == "agent_status"
                && let Knowledge::Known { value } = &native.status
                && value == "idle"
            {
                idle = true;
                break;
            }
        }
    }
    append_evidence(&format!(
        "- idle={} hook={} elapsed={}ms events={:?}",
        idle,
        hook,
        t0.elapsed().as_millis(),
        events
    ));
    assert!(idle, "pty did not reach idle");

    let read = client
        .agent_read(AgentReadParams {
            target: agent,
            source: ReadSource::RecentUnwrapped,
            lines: Some(80),
            format: ReadFormat::Text,
            strip_ansi: true,
        })
        .await
        .expect("agent.read");
    let screen = read.text();
    let preview: String = screen.chars().take(400).collect();
    append_evidence(&format!(
        "- agent.read (redacted preview, {} chars): `{}`",
        screen.len(),
        preview.replace('`', "'")
    ));

    if let Some(path) = driver.session_transcript().await {
        let p = PathBuf::from(&path);
        append_evidence(&format!(
            "- jsonl exists={} {}",
            p.is_file(),
            jsonl_type_histogram(&p)
        ));
        assert!(p.is_file(), "SessionStart transcript_path is not a file");
    } else {
        let meta = launch.join("session-meta.json");
        append_evidence(&format!(
            "- session-meta.json exists={} body={}",
            meta.is_file(),
            fs::read_to_string(&meta).unwrap_or_default()
        ));
        assert!(
            meta.is_file(),
            "SessionStart hook did not write session-meta.json"
        );
    }

    driver.close().await.expect("pty close");
    cleanup_herdr_session();
    append_evidence(&format!(
        "- close + `herdr session stop/delete remuda-test` total {}ms",
        t0.elapsed().as_millis()
    ));
}

#[ignore = "live: claude --bg --model haiku once, then claude stop"]
#[tokio::test]
async fn live_claude_bg_start_prompt_idle_read_close() {
    let root = PathBuf::from("/tmp/remuda-driver");
    let cwd = root.join("bg-live");
    let launch = root.join("bg-launch");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&launch).unwrap();

    let driver = ClaudeBgDriver::new(ClaudeBgOptions {
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: isolated_native_home(),
        binary: BinarySource::Command("claude".into()),
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

    let t0 = Instant::now();
    append_evidence("\n### bg\n");
    let mut handle = driver
        .start(spec(&cwd, DriverKind::ClaudeBg))
        .await
        .expect("bg prepare");
    append_evidence(&format!(
        "- prepare in {}ms dispatch={:?} argv has --bg={} --session-id={}",
        t0.elapsed().as_millis(),
        handle.ack().dispatch,
        handle.recipe().argv.iter().any(|t| t == "--bg"),
        handle.recipe().argv.iter().any(|t| t == "--session-id")
    ));

    let t_send = Instant::now();
    let ack = driver.send(prompt()).await.expect("bg first send");
    append_evidence(&format!(
        "- first send (deferred argv) in {}ms ids={:?}",
        t_send.elapsed().as_millis(),
        ack.native_ids
    ));

    let mut idle = false;
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(800), handle.recv()).await;
        let Ok(Some(obs)) = next else {
            continue;
        };
        if let ObservationPayload::Lifecycle(payload) = &obs.body
            && let LifecyclePayload::Native(native) = payload.as_ref()
            && let Knowledge::Known { value } = &native.status
            && (value == "idle" || value == "backgrounded")
        {
            idle = true;
            append_evidence(&format!(
                "- job lifecycle name={} status={value}",
                native.native_name
            ));
            if value == "idle" {
                break;
            }
        }
    }
    append_evidence(&format!(
        "- reached idle/backgrounded={idle} elapsed={}ms",
        t0.elapsed().as_millis()
    ));

    let job_id = ack.native_ids.get("jobId").cloned().unwrap_or_default();
    let state = isolated_native_home()
        .join("jobs")
        .join(&job_id)
        .join("state.json");
    if state.is_file() {
        let raw = fs::read_to_string(&state).unwrap_or_default();
        let redacted = raw
            .chars()
            .take(500)
            .collect::<String>()
            .replace(&std::env::var("HOME").unwrap_or_default(), "$HOME");
        append_evidence(&format!("- jobs/{job_id}/state.json: `{redacted}`"));
    }
    let meta = launch.join("session-meta.json");
    append_evidence(&format!(
        "- session-meta.json exists={} body={}",
        meta.is_file(),
        fs::read_to_string(&meta)
            .unwrap_or_default()
            .replace(&std::env::var("HOME").unwrap_or_default(), "$HOME")
    ));
    if let Some(sid) = ack.native_ids.get("sessionId") {
        append_evidence(&format!("- sessionId={sid}"));
    }

    driver.close().await.expect("bg stop");
    append_evidence(&format!(
        "- `claude stop` via close() total {}ms (never rm)",
        t0.elapsed().as_millis()
    ));
}
