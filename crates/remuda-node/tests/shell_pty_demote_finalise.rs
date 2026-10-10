//! c-ctxusage r6 item 4(a)/(b): real promoter-loop finalise coverage.
//!
//! (a) `ShellPtyDriver::close` where the LAST seeded assistant record has
//!     usage and NO stop_reason — only the loop-end finalise (not a prior
//!     poll) can publish it. Cooperative path: the backlog is small, so the
//!     close catches a settled pump.
//! (b) A DEMOTE transition through the real loop: the fake harness exits
//!     cleanly via `/exit` WITHOUT appending any transcript record (a command,
//!     not a turn), after binding a transcript whose last run is un-finalised.
//!     The promoter detects the foreground leaving and finalises on the
//!     demote/vanish boundary — the driver is never closed first.
//!
//! Every emitted usage observation is projected through the REAL Hub
//! projection + insertion path and asserted as a durable turn row.
#![cfg(unix)]

use remuda_driver::shell_pty::{AgentLaunch, HookConfig, ShellPtyDriver, ShellPtyOptions};
use remuda_driver::{
    Delegation, Driver, DriverKind, LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_hub::usage_store_test_support::JournalRecord;
use remuda_hub::usage_store_test_support::{
    insert_usage_event, migrate, project_usage_event, rollup_instance,
};
use remuda_protocol::{
    AgentKind, ContentBlock, DriverInput, InstanceSpec, Observation, ObservationPayload,
    PromptInput, TextBlock,
};
use remuda_testing::ensure_workspace_bin;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

const FINAL_MSG_ID: &str = "msg_demote_final_usage";
const SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000004".parse().unwrap(),
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

/// Small closed-turn backlog ending in one un-finalised no-stop run.
fn filler_pair(index: u32) -> String {
    let assistant = serde_json::json!({
        "type": "assistant",
        "uuid": format!("d-a-{index}"),
        "timestamp": "2026-10-09T00:00:00.000Z",
        "isSidechain": false,
        "message": {
            "id": format!("d-msg-{index}"),
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "closed turn"}],
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 20,
                      "cache_creation_input_tokens": 1, "output_tokens": 3}
        }
    });
    format!("{assistant}\n")
}

fn final_record() -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": "d-final-record",
        "timestamp": "2026-10-09T00:01:00.000Z",
        "isSidechain": false,
        "message": {
            "id": FINAL_MSG_ID,
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": null,
            "content": [{"type": "text", "text": "UNFINISHED"}],
            "usage": {
                "input_tokens": 555,
                "cache_read_input_tokens": 777,
                "cache_creation_input_tokens": 7,
                "output_tokens": 9,
                "cache_creation": {"ephemeral_5m_input_tokens": 7,
                                   "ephemeral_1h_input_tokens": 0}
            }
        }
    });
    format!("{record}\n")
}

fn seed_transcript(path: &Path, pairs: u32) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap()).expect("project dir");
    let mut file = std::fs::File::create(path).expect("create transcript");
    for index in 0..pairs {
        file.write_all(filler_pair(index).as_bytes()).unwrap();
    }
    file.write_all(final_record().as_bytes()).unwrap();
    file.sync_all().unwrap();
}

fn to_journal_record(observation: &Observation) -> JournalRecord {
    JournalRecord {
        instance_id: observation.instance_id.as_id().as_str().to_string(),
        seq: observation.seq.0 as i64,
        event_id: observation.event_id.as_id().as_str().to_string(),
        event: serde_json::to_value(observation).expect("serialize"),
        observed_at: String::from(observation.observed_at.clone()),
    }
}

fn assert_durable_usage_row(observation: &Observation) {
    let conn = Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");
    let record = to_journal_record(observation);
    let row = project_usage_event(&record, None, None).expect("projects usage");
    assert_eq!(row.scope, "turn");
    assert_eq!(row.scope_id.as_deref(), Some(FINAL_MSG_ID));
    assert!(insert_usage_event(&conn, &row).expect("insert"));
    assert_eq!(row.input_tokens, Some(555));
    assert_eq!(row.output_tokens, Some(9));
    assert_eq!(row.cache_read_tokens, Some(777));
    assert_eq!(row.cache_write_tokens, Some(7));
    let rollup = rollup_instance(&conn, record.instance_id.as_str(), "claude", None)
        .expect("rollup")
        .expect("rollup exists");
    assert_eq!(rollup.turns, 1);
    assert_eq!(rollup.context_used_tokens, Some(555 + 777 + 7));
}

struct Harness {
    driver: ShellPtyDriver,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

async fn launch_harness() -> (Harness, tokio::sync::mpsc::Receiver<Observation>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let workspace = root.join("workspace");
    let native_home = root.join("home");
    let instance_dir = root.join("instance");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&native_home).unwrap();
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).unwrap();
    let binary = install.join("claude");
    std::fs::copy(ensure_workspace_bin("fake-harness"), &binary).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let events_path = root.join("harness-events.jsonl");

    let transcript = remuda_driver::claude_transcript::project_dir(&native_home, &workspace)
        .join(format!("{SESSION_ID}.jsonl"));
    seed_transcript(&transcript, 20);

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
    options.pin_native_home = true;
    options.claude_home = Some(native_home);
    options
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());
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
    let rx = handle.into_events();

    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    while std::time::Instant::now() < deadline {
        if Driver::wait_control(&driver).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(Driver::wait_control(&driver).await.is_ok(), "never ready");
    (Harness { driver, dir }, rx)
}

fn input_text(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: remuda_protocol::PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: remuda_protocol::InputOrigin::Human,
        native_client_message_id: "probe".into(),
    }))
}

async fn tty_send(driver: &ShellPtyDriver, text: &str) {
    // No interaction approval: a raw tty write via the driver's send input.
    driver.send(input_text(text)).await.expect("send");
}

async fn wait_for_scope(
    rx: &mut tokio::sync::mpsc::Receiver<Observation>,
    scope: &str,
    secs: u64,
) -> Option<Observation> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let observation = tokio::time::timeout(remaining, rx.recv()).await.ok()??;
        if let ObservationPayload::Usage(payload) = &observation.body
            && payload.scope_id == scope
        {
            return Some(observation);
        }
    }
}

/// (a) Close finalises the buffered no-stop run; the row projects and inserts
/// through the real Hub pipeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_pty_close_finalises_the_no_stop_usage_run() {
    let (harness, mut rx) = launch_harness().await;
    // Wait until the seeded replay pumped its closed turns: the un-finalised
    // run is then the only thing buffered. Event sync, no wall-clock assert.
    assert!(
        wait_for_scope(&mut rx, "d-msg-19", 30).await.is_some(),
        "the seeded replay never pumped"
    );

    harness.driver.close().await.expect("close");
    let observation = wait_for_scope(&mut rx, FINAL_MSG_ID, 30)
        .await
        .expect("close did not finalise the buffered no-stop run");
    assert_durable_usage_row(&observation);
}

/// (b) The foreground harness exits via `/exit` (no transcript append); the
/// real promoter loop demotes and finalises the buffered run BEFORE the
/// driver is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_pty_demote_finalises_the_no_stop_usage_run() {
    let (harness, mut rx) = launch_harness().await;
    assert!(
        wait_for_scope(&mut rx, "d-msg-19", 30).await.is_some(),
        "the seeded replay never pumped"
    );

    // `/exit` is a harness command (not a turn), so it appends no transcript
    // record: the seeded no-stop run stays buffered until the demote.
    tty_send(&harness.driver, "/exit").await;

    let observation = wait_for_scope(&mut rx, FINAL_MSG_ID, 30)
        .await
        .expect("demote did not finalise the buffered no-stop run");
    assert_durable_usage_row(&observation);

    // Teardown only after the demote finalise was observed — proving close was
    // not what published it.
    harness.driver.close().await.expect("close");
}
