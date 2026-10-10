//! c-ctxusage r6 item 3: when close lands while the promoter's first-bind
//! pump is still mapping a LARGE backlog past the bounded wait (15 s), the
//! poller task is aborted. The abort-path FinaliseGuard owns the hydrator
//! through the loop's shared slot (and the pump/finalise emission queue), so
//! its Drop still runs the mapper's `finish()` and rescues the LAST buffered
//! assistant run (usage present, stop_reason null), stamped through the
//! promoter builder. Before the fix the guard held a separate `None`, so that
//! usage never reached usage_events.
//!
//! The backlog is written to the transcript BEFORE the harness launches, so
//! the first-bind replay pump is the long map close aborts into. The final
//! record carries no `stop_reason`, so an ordinary poll flush can NEVER
//! publish it: only `finish()` (cooperative finalise or the guard) can. The
//! harness is kept alive with one harmless unmatched prompt; nothing ever
//! supersedes the seeded run.
//!
//! No wall-clock is asserted anywhere; the backlog size is content, not a
//! timing assumption (a fast box takes the cooperative finalise path, which
//! publishes the same row — the assertion is identical either way).
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
    AgentKind, ContentBlock, DriverInput, InstanceSpec, Observation, PromptInput, TextBlock,
};
use remuda_testing::ensure_workspace_bin;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const FINAL_MSG_ID: &str = "msg_abort_final_usage";
/// The fake harness's fixed session id (`remuda_testing::paths::FIXTURE`).
const SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";

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

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000003".parse().unwrap(),
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

/// One realistic assistant/user pair (thinking + text + tool call + tool
/// result), distinct ids so every mapper code path really runs. ~1.5 KiB and
/// ~0.3 ms to map; the default pair count makes the first-bind replay a
/// ~20-30 s map that pushes close past its 15 s bound on a loaded gate box.
fn filler_pair(seed: &str, index: u32) -> String {
    let assistant = serde_json::json!({
        "type": "assistant",
        "uuid": format!("{seed}-a-{index}"),
        "timestamp": "2026-10-09T00:00:00.000Z",
        "isSidechain": false,
        "message": {
            "id": format!("{seed}-msg-{index}"),
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "t".repeat(800),
                 "signature": "s".repeat(300)},
                {"type": "text", "text": "x".repeat(800)},
                {"type": "tool_use", "id": format!("{seed}-toolu-{index}"),
                 "name": "Bash", "input": {"command": "echo filler"}}
            ],
            "usage": {
                "input_tokens": 100 + index % 50,
                "cache_read_input_tokens": 5_000,
                "cache_creation_input_tokens": 10,
                "output_tokens": 20
            }
        }
    });
    let user = serde_json::json!({
        "type": "user",
        "uuid": format!("{seed}-u-{index}"),
        "timestamp": "2026-10-09T00:00:00.000Z",
        "message": {
            "role": "user",
            "content": [{"type": "tool_result",
                "tool_use_id": format!("{seed}-toolu-{index}"),
                "content": "out".repeat(200), "is_error": false}]
        }
    });
    format!("{assistant}\n{user}\n")
}

/// The record only a `finish()` can publish: a fresh assistant message id,
/// real counters, stop_reason explicitly null.
fn final_record() -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": "abort-final-record",
        "timestamp": "2026-10-09T00:01:00.000Z",
        "isSidechain": false,
        "message": {
            "id": FINAL_MSG_ID,
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": null,
            "content": [{"type": "text", "text": "FINAL_UNFINISHED_TURN"}],
            "usage": {
                "input_tokens": 7_777,
                "cache_read_input_tokens": 9_999,
                "cache_creation_input_tokens": 66,
                "output_tokens": 88,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 66,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        }
    });
    format!("{record}\n")
}

/// Pre-seed the transcript the harness will bind: a big closed-turn backlog
/// ending in ONE un-finalised run. The harness opens with create+append, so
/// the content survives.
fn seed_transcript(path: &Path, pairs: u32) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap()).expect("project dir");
    let mut file = std::fs::File::create(path).expect("create transcript");
    for index in 0..pairs {
        file.write_all(filler_pair("fill", index).as_bytes())
            .expect("write filler");
    }
    file.write_all(final_record().as_bytes())
        .expect("write final record");
    file.sync_all().expect("sync seed");
}

fn to_journal_record(observation: &Observation) -> JournalRecord {
    JournalRecord {
        instance_id: observation.instance_id.as_id().as_str().to_string(),
        seq: observation.seq.0 as i64,
        event_id: observation.event_id.as_id().as_str().to_string(),
        event: serde_json::to_value(observation).expect("serialize observation"),
        observed_at: String::from(observation.observed_at.clone()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_past_the_finalise_bound_still_rescues_the_last_usage_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    if std::env::var("KEEP_ABORT_E2E").is_ok() {
        eprintln!("ABORT_E2E dir retained: {root:?}");
        std::mem::forget(dir);
    }
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

    // Pre-seed before the harness launches: the first-bind replay is the long
    // pump close aborts into.
    let transcript = remuda_driver::claude_transcript::project_dir(&native_home, &workspace)
        .join(format!("{SESSION_ID}.jsonl"));
    let pairs: u32 = std::env::var("ABORT_BACKLOG_PAIRS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60_000);
    seed_transcript(&transcript, pairs);

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
    // Mirror NativeDriverConfig: pin the child config dir and the promoter
    // scan home to the managed home holding the pre-seeded transcript.
    options.pin_native_home = true;
    options.claude_home = Some(native_home.clone());
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

    // Drain from the moment the runhandle exists: the first-bind replay is an
    // observation storm, and the guard's best-effort send needs a live
    // receiver. Keep only the rescued run, by scope id.
    let handle = driver.start(spec_for(&workspace)).await.expect("start");
    let mut events_rx = handle.into_events();
    let rescued = Arc::new(Mutex::new(None::<Observation>));
    let drainer_rescued = Arc::clone(&rescued);
    // Signals the promoter bound the seeded transcript (hydrator exists and
    // the first-bind pump is running or about to block-map the seed).
    // Lifecycle emitted directly, before any mapped frames, so it arrives
    // even when the backlog takes minutes to map under gate load.
    let pump_started = Arc::new(AtomicBool::new(false));
    let drainer_pump = Arc::clone(&pump_started);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let remuda_protocol::ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
            {
                drainer_pump.store(true, Ordering::SeqCst);
            }
            if let remuda_protocol::ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id == FINAL_MSG_ID
            {
                *drainer_rescued.lock().unwrap() = Some(observation);
            }
        }
    });

    // The promoter is running (SessionStart binds the seeded transcript).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if Driver::wait_control(&driver).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ready, "composer never became ready");

    // Keep the harness (and its PTY stdin) alive with one harmless unmatched
    // prompt — the fake runs its fast tool-less echo turn and keeps waiting.
    // It opens its transcript create+append (no truncation), so the seeded
    // replay survives and its appended echo records land behind the final
    // run, never superseding it.
    driver.send(prompt("KEEPALIVE")).await.expect("send");

    // Event sync, no wall-clock assertion: wait for the bind lifecycle.
    let pump_ready = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while !pump_started.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(pump_ready.is_ok(), "the seeded replay pump never started");

    // Close while the first-bind replay is still mapping the backlog. The
    // poller aborts past the bound; the guard must finish the buffered run.
    driver.close().await.expect("close");

    // The guard fires once the detached map reaches completion and the
    // final run is parked, so wait on the EVENT (a very loaded gate box can
    // spend minutes block-mapping a ~120k-record replay; this is an event
    // bound, not a wall-clock assertion).
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(400), async {
        loop {
            if let Some(observation) = rescued.lock().unwrap().take() {
                break observation;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    drainer.abort();
    let Ok(rescued) = outcome else {
        panic!(
            "the aborted close did not finalise the buffered last run \
             ({FINAL_MSG_ID} missing from emitted usage)"
        )
    };

    // Project the rescued driver observation through the REAL Hub projection
    // + insertion path and assert the durable row, exactly as the Hub's
    // journal append would.
    let conn = Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");
    let record = to_journal_record(&rescued);
    let row = project_usage_event(&record, None, None).expect("projects usage");
    assert_eq!(row.scope, "turn");
    assert_eq!(row.scope_id.as_deref(), Some(FINAL_MSG_ID));
    assert!(insert_usage_event(&conn, &row).expect("insert usage row"));
    assert_eq!(row.input_tokens, Some(7_777));
    assert_eq!(row.output_tokens, Some(88));
    assert_eq!(row.cache_read_tokens, Some(9_999));
    assert_eq!(row.cache_write_tokens, Some(66));
    let rollup = rollup_instance(&conn, record.instance_id.as_str(), "claude", None)
        .expect("rollup query")
        .expect("rollup exists");
    assert_eq!(rollup.turns, 1);
    assert_eq!(
        rollup.context_used_tokens,
        Some(7_777 + 9_999 + 66),
        "next-request context = input + cache read + cache creation"
    );
}
