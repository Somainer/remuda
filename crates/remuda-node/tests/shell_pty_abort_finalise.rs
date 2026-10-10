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
use remuda_protocol::{AgentKind, InstanceSpec, Observation};
use remuda_testing::ensure_workspace_bin;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const FINAL_MSG_ID: &str = "msg_abort_final_usage";
/// The fake harness's fixed session id (`remuda_testing::paths::FIXTURE`).
const SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";

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

/// One lightweight closed-turn record — kept cheap so the seeded backlog is
/// large enough to make the first-bind replay long (well past close's 15 s
/// bound) without costing minutes per test under gate load. One record is
/// enough: distinct ids make every mapper code path run across pairs, and
/// the assertion only needs the last un-finalised run at the tail.
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
                {"type": "text", "text": "x"}
            ],
            "usage": {
                "input_tokens": 100 + index % 50,
                "cache_read_input_tokens": 5_000,
                "cache_creation_input_tokens": 10,
                "output_tokens": 20
            }
        }
    });
    format!("{assistant}\n")
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

// Test-only file-backed collect gate makes this abort deterministic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_past_the_finalise_bound_still_rescues_the_last_usage_run() {
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

    // Pre-seed before the harness launches: the first-bind replay is the long
    // pump close aborts into.
    let transcript = remuda_driver::claude_transcript::project_dir(&native_home, &workspace)
        .join(format!("{SESSION_ID}.jsonl"));
    // The abort gate makes backlog size irrelevant: a handful of closed-turn
    // records is enough (the collect parks before reading them regardless).
    seed_transcript(&transcript, 20);
    // Test-only file-backed seams live next to the transcript (found by file
    // name, not process env): the collect gate parks the first-bind pump
    // before it reads the tail, and the skip marker makes close's cooperative
    // finalise a no-op so ONLY the abort guard rescues the buffered run.
    // Created before the driver starts; the gate is removed only after
    // close() aborts the mid-map promoter task.
    let transcript_gate = transcript.with_file_name(".remuda-test-collect-gate");
    let transcript_skip = transcript.with_file_name(".remuda-test-skip-coop-finalise");
    std::fs::write(&transcript_gate, b"hold").unwrap();
    std::fs::write(&transcript_skip, b"skip").unwrap();

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

    // Drain observations from the moment the runhandle exists. A file-backed
    // test gate parks the first-bind blocking collect until the test removes
    // it, so close() deterministically aborts the promoter task WHILE the map
    // is in flight — no reliance on backlog size or wall-clock timing. Keep
    // only the rescued run, by scope id.
    let handle = driver.start(spec_for(&workspace)).await.expect("start");
    let mut events_rx = handle.into_events();
    let rescued = Arc::new(Mutex::new(None::<Observation>));
    let drainer_rescued = Arc::clone(&rescued);
    // Set once the pump's blocking collect is parked on the gate — i.e. the
    // hydrator is OUT of the slot and map_in_flight is true, exactly the state
    // close() must abort into.
    let pump_parked = Arc::new(AtomicBool::new(false));
    let drainer_parked = Arc::clone(&pump_parked);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let remuda_protocol::ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
            {
                drainer_parked.store(true, Ordering::SeqCst);
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

    // Wait for the bind; the collect gate then keeps the FIRST-bind map
    // parked (no user prompt is sent, so nothing appends to or supersedes the
    // seeded FINAL run — its only publisher is the abort-path finish).
    let pump_ready = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !pump_parked.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(pump_ready.is_ok(), "the seeded replay never bound");
    // Give the blocking collect a beat to reach its gate after the bind.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The no-stop FINAL usage must NOT be present before close: the gate has
    // parked the collect before tail.poll, so flush/finish have never run and
    // the run is buffered in the mapper. Drain the channel and assert.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        rescued.lock().unwrap().is_none(),
        "FINAL usage published before close — the test no longer exercises the abort path"
    );

    // Close while the collect is parked on the gate: close aborts the
    // promoter task mid-map. Cooperative finalise cannot run; the abort guard
    // must finish the buffered run after the gate releases.
    driver.close().await.expect("close");
    // Release the detached blocking collect. It completes, parks the hydrator
    // back; the guard's blocking thread (waiting on collect_in_flight) then
    // finishes the retained no-stop run.
    std::fs::remove_file(&transcript_gate).unwrap();
    std::fs::remove_file(&transcript_skip).ok();

    // Wait on the rescued USAGE event (event-synced, generous bound for a
    // loaded box; the gate made the abort deterministic, so this is fast).
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if let Some(observation) = rescued.lock().unwrap().take() {
                break observation;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
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
