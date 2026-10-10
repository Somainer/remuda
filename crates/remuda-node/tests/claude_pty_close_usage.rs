//! c-ctxusage r7 item 4(c): `ClaudePtyDriver::close` over a seeded transcript
//! publishes a buffered no-stop usage run whose LAST assistant record has
//! usage with stop_reason=null, and the observation survives the REAL Hub
//! projection + insertion (project_usage_event / insert_usage_event) as a
//! durable turn row.
//!
//! Uses the production claude-pty driver against fake-herdr (same harness as
//! the driver integration suite), but lives in remuda-node so it can call the
//! Hub usage pipeline (remuda-hub is a dev-dependency here).

use remuda_driver::{
    BinarySource, ClaudePtyDriver, ClaudePtyOptions, Delegation, Driver, DriverKind, LaunchOrigin,
    ProviderHealth, ProviderKind, ProviderProfile, pin_binary,
};
use remuda_hub::usage_store_test_support::{
    JournalRecord, insert_usage_event, migrate, project_usage_event, rollup_instance,
};
use remuda_protocol::{InstanceSpec, Knowledge, Observation, ObservationPayload};
use remuda_testing::{
    FakeHerdrOptions, FakeHerdrServer, ShortTempDir, ensure_workspace_bin, install_executable,
};
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::time::Duration;

const FINAL_MSG_ID: &str = "msg_claude_pty_final_usage";

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000006".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn spec(cwd: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec = serde_json::from_str(include_str!(
        "../../remuda-driver/tests/fixtures/instance-spec.json"
    ))
    .unwrap();
    spec.driver = DriverKind::ClaudePty;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("haiku".into());
    spec
}

fn filler(index: u32) -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": format!("cp-a-{index}"),
        "timestamp": "2026-10-09T00:00:00.000Z",
        "isSidechain": false,
        "message": {
            "id": format!("cp-msg-{index}"),
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "x"}],
            "usage": {
                "input_tokens": 11,
                "cache_read_input_tokens": 22,
                "cache_creation_input_tokens": 1,
                "output_tokens": 3
            }
        }
    });
    format!("{record}\n")
}

fn final_record() -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": "cp-final-record",
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
                "input_tokens": 432,
                "cache_read_input_tokens": 654,
                "cache_creation_input_tokens": 5,
                "output_tokens": 7,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 5,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        }
    });
    format!("{record}\n")
}

fn seed_transcript(path: &Path, pairs: u32) {
    use std::io::Write;
    let mut file = fs::File::create(path).expect("create transcript");
    for index in 0..pairs {
        file.write_all(filler(index).as_bytes()).unwrap();
    }
    file.write_all(final_record().as_bytes()).unwrap();
    file.sync_all().unwrap();
}

fn to_journal_record(observation: &Observation) -> JournalRecord {
    JournalRecord {
        instance_id: "ins_cp_close".into(),
        seq: observation.seq.0 as i64,
        event_id: format!("evt_cp_{}", observation.seq.0),
        observed_at: String::from(observation.observed_at.clone()),
        event: serde_json::to_value(observation).expect("serialize observation"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_pty_close_finalises_no_stop_usage_and_projects_to_hub_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_root = ShortTempDir::new().unwrap();
    let socket_dir = socket_root.as_ref().join("herdr");
    fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join("herdr.sock");
    let fake_bin = ensure_workspace_bin("fake-herdr");
    let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();

    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let claude = install_executable(
        tmp.path(),
        "claude",
        "#!/bin/sh\necho '2.1.268 (Claude Code)'\n",
    );

    // Seed the transcript the pump binds via session-meta before it starts.
    fs::create_dir_all(&launch).unwrap();
    let transcript = launch.join("transcript.jsonl");
    // A small closed-turn backlog plus the no-stop final record; close timing
    // is asserted on events, not on the mapping duration.
    seed_transcript(&transcript, 30);

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-close".into(),
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

    let mut handle = driver.start(spec(&cwd)).await.expect("start");
    fs::write(
        launch.join("session-meta.json"),
        serde_json::json!({
            "session_id": "fixture-session",
            "transcript_path": transcript,
            "hook_event_name": "SessionStart",
        })
        .to_string(),
    )
    .unwrap();

    // Wait for the bind, event-driven (no idle-gap sleeps).
    let mut bound = false;
    let bound_deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < bound_deadline {
        let obs = match tokio::time::timeout(Duration::from_millis(500), handle.recv()).await {
            Ok(Some(obs)) => obs,
            _ => continue,
        };
        if let ObservationPayload::Lifecycle(payload) = &obs.body
            && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref()
            && native.native_name == "transcript_bound"
        {
            bound = true;
            break;
        }
    }
    assert!(bound, "transcript never bound");

    // Close: cooperative finalise (fast box) or the abort guard (slow box).
    driver.close().await.expect("close");

    // Drain until the no-stop usage is delivered.
    let mut rescued: Option<Observation> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while tokio::time::Instant::now() < deadline {
        let obs = match tokio::time::timeout(Duration::from_secs(5), handle.recv()).await {
            Ok(Some(obs)) => obs,
            _ => break,
        };
        if let ObservationPayload::Usage(payload) = &obs.body
            && payload.scope_id == FINAL_MSG_ID
        {
            rescued = Some(obs);
            break;
        }
    }
    let rescued = rescued.expect("close did not finalise the no-stop usage");
    // Project through the REAL Hub pipeline BEFORE moving the body out.
    let conn = Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");
    let record = to_journal_record(&rescued);
    let row = project_usage_event(&record, None, None).expect("projects usage");
    let payload = match rescued.body {
        ObservationPayload::Usage(payload) => payload,
        _ => unreachable!(),
    };
    let known = |value: Knowledge<remuda_protocol::U64>| match value {
        Knowledge::Known { value } => value.0,
        other => panic!("expected Known, got {other:?}"),
    };
    assert_eq!(payload.scope_id, FINAL_MSG_ID);
    assert_eq!(known(payload.input_tokens), 432);
    assert_eq!(known(payload.output_tokens), 7);
    assert_eq!(known(payload.cache_read_tokens), 654);
    assert_eq!(known(payload.cache_write_tokens), 5);

    // Durable turn row asserted via the real Hub pipeline.
    assert_eq!(row.scope, "turn");
    assert_eq!(row.scope_id.as_deref(), Some(FINAL_MSG_ID));
    assert!(insert_usage_event(&conn, &row).expect("insert usage row"));
    assert_eq!(row.input_tokens, Some(432));
    assert_eq!(row.output_tokens, Some(7));
    assert_eq!(row.cache_read_tokens, Some(654));
    assert_eq!(row.cache_write_tokens, Some(5));
    let rollup = rollup_instance(&conn, "ins_cp_close", "claude", None)
        .expect("rollup query")
        .expect("rollup exists");
    assert_eq!(rollup.turns, 1);
    assert_eq!(
        rollup.context_used_tokens,
        Some(432 + 654 + 5),
        "context = input + cache read + cache creation"
    );
}
