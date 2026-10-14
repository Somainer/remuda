//! c-usagefu r9 item 7: the close -> resume adoption path driven through the
//! REAL ShellPtyDriver (not mutex slots and synthetic tasks).
//!
//! A hook-configured codex launch starts and closes; resume spawns the
//! continued run and adopts its file-tail adapter into the live driver.
//! A native codex rollout carrying a token_usage_record is appended to the
//! shadow CODEX_HOME ONLY AFTER resume; the Usage observation must arrive on
//! the resumed run's returned channel — the exact regression the r8 helper
//! test could not exercise (removing the production adopt call kept it green).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use remuda_driver::shell_pty::{AgentLaunch, HookConfig, ShellPtyDriver, ShellPtyOptions};
use remuda_driver::{
    Delegation, Driver, DriverKind, LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_protocol::{
    AgentKind, ContentBlock, DriverInput, InputOrigin, InstanceId, InstanceSpec, Knowledge,
    ObservationPayload, PromptInput, PromptMode, TextBlock,
};

/// A pinned fake codex: answers the binary pin's --version probe, ignores the
/// codex argv, and stays alive so the resumed run can accept a real prompt.
fn fake_codex_script(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-codex-resume-usage.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\ncase \"$1\" in --version|version) echo 'codex 0.154.0'; exit 0;; esac\nexec sleep 120\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn options_for(dir: &Path, instance_id: &InstanceId, spec: &InstanceSpec) -> ShellPtyOptions {
    let instance_dir = dir.join(instance_id.as_id().as_str());
    let mut options = ShellPtyOptions::agent(
        dir.to_path_buf(),
        AgentKind::Codex,
        AgentLaunch {
            profile: Box::new(ProviderProfile {
                id: spec.provider_profile.id.clone(),
                kind: ProviderKind::OpenaiResponses,
                base_url: String::new(),
                delegation: Delegation::None,
                secret_ref: None,
                models: vec!["gpt-5.4".into()],
                health: ProviderHealth::Healthy,
            }),
            launch_dir: instance_dir.join("launch"),
            native_home: dir.join("codex-native-home"),
            // The child never has to be a real codex: the file-tail adapter
            // reads the shadow CODEX_HOME directly. The script answers the
            // version pin and sleeps, so the resumed run is alive to receive
            // the production prompt send below.
            binary: Some(fake_codex_script(dir)),
            origin: LaunchOrigin::Human,
            native_home_managed: true,
            settings_overlay: None,
        },
    );
    options.instance_id = Some(instance_id.clone());
    options.hooks = Some(HookConfig {
        instance_dir,
        // Never executed by the fake child; embedded only in hook config.
        relay_binary: PathBuf::from("/bin/true"),
        tui: remuda_driver::launch::TuiMode::Default,
    });
    options
}

fn spec_for(dir: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = DriverKind::ShellPty;
    spec.kind = AgentKind::Codex;
    spec.cwd = dir.to_string_lossy().into_owned();
    spec.model_id = Some("gpt-5.4".into());
    spec
}

/// Write the session's ORIGINAL rollout exactly as the first codex run does:
/// session_meta with the original timestamp, the real first user prompt, and
/// one priced turn. c-usagefu r15 item 3: a real resume APPENDS to this file,
/// never creating a fresh one.
fn write_original_rollout(dir: &Path, instance_id: &InstanceId, session_id: &str) {
    let home = dir
        .join(instance_id.as_id().as_str())
        .join("launch")
        .join("codex-home");
    let session_dir = home.join("sessions/2026/10/12");
    std::fs::create_dir_all(&session_dir).unwrap();
    let at = time::OffsetDateTime::now_utc();
    let at = at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let body = format!(
        "{{\"timestamp\":\"{at}\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{sid}\",\"session_id\":\"{sid}\",\"cwd\":\"{cwd}\",\"timestamp\":\"{at}\"}}}}\n\
         {{\"timestamp\":\"{at}\",\"ordinal\":1,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m_env\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"<environment_context>\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"environments.environment_context\"]}}}}}}\n\
         {{\"timestamp\":\"{at}\",\"ordinal\":2,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m_u\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"go\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"user.text\"]}}}}}}\n\
         {{\"timestamp\":\"{at}\",\"ordinal\":3,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"t1\"}}}}\n\
         {{\"timestamp\":\"{at}\",\"ordinal\":4,\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"t1\",\"response_id\":\"r1\",\"usage\":{{\"input_tokens\":50,\"cached_input_tokens\":50,\"cache_creation_input_tokens\":0,\"output_tokens\":20,\"reasoning_output_tokens\":0,\"total_tokens\":70}}}}}}\n\
         {{\"timestamp\":\"{at}\",\"ordinal\":5,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t1\"}}}}\n",
        sid = session_id,
        cwd = dir.display(),
    );
    std::fs::write(
        session_dir.join(format!("rollout-{session_id}.jsonl")),
        body,
    )
    .unwrap();
    std::fs::write(
        home.join("session_index.jsonl"),
        format!("{{\"id\":\"{session_id}\"}}\n"),
    )
    .unwrap();
}

/// Append the resumed run's NEW turn to the existing rollout — codex resume
/// leaves the original session_meta untouched and only appends records.
fn append_resumed_turn(dir: &Path, instance_id: &InstanceId, session_id: &str) {
    let home = dir
        .join(instance_id.as_id().as_str())
        .join("launch")
        .join("codex-home");
    let path = home
        .join("sessions/2026/10/12")
        .join(format!("rollout-{session_id}.jsonl"));
    let at = time::OffsetDateTime::now_utc();
    let at = at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    use std::io::Write as _;
    writeln!(
        file,
        "{{\"timestamp\":\"{at}\",\"ordinal\":6,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m_u2\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"go\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"user.text\"]}}}}}}}}"
    )
    .unwrap();
    writeln!(
        file,
        "{{\"timestamp\":\"{at}\",\"ordinal\":7,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"t2\"}}}}"
    )
    .unwrap();
    writeln!(
        file,
        "{{\"timestamp\":\"{at}\",\"ordinal\":8,\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"t2\",\"response_id\":\"r2\",\"usage\":{{\"input_tokens\":60,\"cached_input_tokens\":60,\"cache_creation_input_tokens\":0,\"output_tokens\":30,\"reasoning_output_tokens\":0,\"total_tokens\":90}}}}}}"
    )
    .unwrap();
    writeln!(
        file,
        "{{\"timestamp\":\"{at}\",\"ordinal\":9,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"t2\"}}}}"
    )
    .unwrap();
}

#[tokio::test]
async fn close_resume_adopted_tail_emits_native_usage_appended_after_resume() {
    let dir = tempfile::tempdir().unwrap();
    let instance_id = InstanceId::new();
    let session_id = "r9-item7-session";
    let spec = spec_for(dir.path());

    // START: real driver launch with a hook-configured codex shadow home.
    let driver = ShellPtyDriver::new(options_for(dir.path(), &instance_id, &spec));
    let mut first = driver.start(spec.clone()).await.expect("first start");
    // Drain whatever the first run emits.
    let _ = tokio::time::timeout(Duration::from_millis(300), first.recv()).await;
    // The ORIGINAL rollout exists while the first run is alive: original
    // session_meta timestamp with the first turn. codex resume appends to
    // THIS file instead of creating a new one.
    write_original_rollout(dir.path(), &instance_id, session_id);

    // CLOSE: kills the child, aborts the first run's adapter tasks, unbinds
    // hooks. The shadow directory survives for the continued run.
    Driver::close(&driver).await.expect("close");
    drop(first);

    // RESUME: start_resumed_inner spawns the continued driver and adopts its
    // file-tail handle into the live slot (the production adoption call).
    // The session id is the native session the run continues; the resumed
    // tail binds the existing rollout BY ID (r15 item 3), as a real
    // `codex resume <id>` does.
    let mut resumed = driver
        .start_resumed(spec.clone(), session_id.to_owned())
        .await
        .expect("resume spawns the continued run");

    // The real prompt is dispatched through the production send path BEFORE
    // the resumed turn's records are appended to the original rollout.
    driver
        .send(DriverInput::Prompt(Box::new(PromptInput {
            mode: PromptMode::NewTurn,
            blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                text: "go".into(),
            }))],
            origin: InputOrigin::Human,
            native_client_message_id: "r9-item7".into(),
        })))
        .await
        .expect("the resumed live child accepts the prompt");
    append_resumed_turn(dir.path(), &instance_id, session_id);

    // The adopted tail must surface the APPENDED turn's Usage observation on
    // the resumed run's channel even though the rollout predates the resume
    // floor. Adapter ticks are 250 ms; allow a generous budget.
    let mut saw_resumed_usage = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, resumed.recv()).await {
            Ok(Some(observation)) => {
                let turn = match &observation.source.native_turn_id {
                    Knowledge::Known { value } => Some(value.as_str()),
                    _ => None,
                };
                if matches!(&observation.body, ObservationPayload::Usage(_)) && turn == Some("t2") {
                    saw_resumed_usage = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_resumed_usage,
        "the adopted codex tail binds the original rollout by id and emits the appended turn"
    );

    Driver::close(&driver).await.expect("final close");
}
