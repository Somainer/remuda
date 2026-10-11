//! c-ctxusage r7 item 4(c): `ClaudePtyDriver::close` over a seeded transcript
//! publishes a buffered no-stop usage run whose LAST assistant record has
//! usage with stop_reason=null, and the observation survives the REAL Hub
//! projection + insertion (project_usage_event / insert_usage_event) as a
//! durable turn row.
//!
//! Uses the production claude-pty driver against fake-herdr (same harness as
//! the driver integration suite), but lives in remuda-node so it can call the
//! Hub usage pipeline (remuda-hub is a dev-dependency here).
//!
//! c-ctxusage r8 item 7: the close-abort path is forced deterministically
//! with the injected fault seam (a barrier INSIDE the first blocking
//! collect, plus a shortened close bound) instead of a load-dependent 30k
//! backlog and a 5 s idle break that could accept a missing rescue or reject
//! a slow-but-valid one.

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
    use remuda_driver::test_faults::TranscriptFaults;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

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

    fs::create_dir_all(&launch).unwrap();
    let transcript = launch.join("transcript.jsonl");
    // Backlog size is irrelevant: the injected barrier parks the FIRST
    // collect, so close deterministically aborts the pump mid-map.
    seed_transcript(&transcript, 30);

    // Barrier INSIDE the first blocking collect + a 500 ms close bound.
    let faults = TranscriptFaults::new();
    faults.arm_pump_gate();
    faults.arm_close_timeout(Duration::from_millis(500));

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
    driver.set_test_faults(faults.clone());

    let handle = driver.start(spec(&cwd)).await.expect("start");
    let mut events_rx = handle.into_events();
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

    let final_count = Arc::new(AtomicU32::new(0));
    let bound = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (final_tx, final_rx) = tokio::sync::oneshot::channel();
    let final_tx = std::sync::Mutex::new(Some(final_tx));
    let drainer_count = Arc::clone(&final_count);
    let drainer_bound = Arc::clone(&bound);
    let drainer = tokio::spawn(async move {
        while let Some(obs) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(payload) = &obs.body
                && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref()
                && native.native_name == "transcript_bound"
            {
                drainer_bound.store(true, AtomicOrdering::SeqCst);
            }
            if let ObservationPayload::Usage(payload) = &obs.body
                && payload.scope_id == FINAL_MSG_ID
            {
                drainer_count.fetch_add(1, AtomicOrdering::SeqCst);
                if let Some(tx) = final_tx.lock().unwrap().take() {
                    let _ = tx.send(obs);
                }
            }
        }
    });

    let bind_deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !bound.load(AtomicOrdering::SeqCst) && std::time::Instant::now() < bind_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bound.load(AtomicOrdering::SeqCst), "transcript never bound");

    // Barrier: the first collect is parked before tail.poll; FINAL is absent.
    assert!(
        faults.wait_pump_entered(),
        "the first blocking collect never entered its barrier"
    );
    assert_eq!(final_count.load(AtomicOrdering::SeqCst), 0);

    driver.close().await.expect("close");
    // Prove the timeout-driven abort actually happened (not a fast-box
    // cooperative path that happened to win).
    assert!(
        faults.wait_abort_residual(),
        "close returned without the abort-path residual running"
    );
    assert_eq!(
        final_count.load(AtomicOrdering::SeqCst),
        0,
        "FINAL published before the parked collect was released"
    );

    // Release the detached first collect; the guard finishes the buffered run
    // and the flusher delivers its no-stop usage exactly once.
    faults.release();
    let rescued = tokio::time::timeout(Duration::from_secs(60), final_rx)
        .await
        .expect("close never rescued the no-stop usage (channel closed)")
        .expect("final oneshot closed without a frame");
    // r9 item 6: keep draining to channel close and prove the FINAL was
    // delivered exactly once, instead of aborting the observer early.
    drainer.await.expect("event drainer panicked");
    assert_eq!(
        final_count.load(AtomicOrdering::SeqCst),
        1,
        "the rescued no-stop usage must be delivered exactly once"
    );

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

/// c-ctxusage r8 item 2: when close's 2 s deadline aborts the pump WHILE the
/// cooperative finish is running, the finish must be part of the shared
/// in-flight state — the abort guard waits for it and the single flusher
/// drains the FINAL frames exactly once. Before the fix the cooperative
/// finish ran with map_in_flight clear and the hydrator already out of the
/// slot: the guard saw "nothing in flight, empty slot", stopped the flusher,
/// and the frames the detached finish parked were never delivered.
///
/// The barrier is INSIDE the cooperative finish: the test holds it past
/// close's (shortened) deadline, proves the timeout-driven abort happened and
/// no FINAL leaked early, releases, and asserts the FINAL arrives once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_pty_abort_mid_cooperative_finish_still_delivers_final_usage_once() {
    use remuda_driver::test_faults::TranscriptFaults;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

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
    fs::create_dir_all(&launch).unwrap();
    let transcript = launch.join("transcript.jsonl");
    seed_transcript(&transcript, 30);

    // Barrier inside the cooperative finish + a 500 ms close bound so the
    // timeout-driven abort is deterministic and fast.
    let faults = TranscriptFaults::new();
    faults.arm_finish_gate();
    faults.arm_close_timeout(Duration::from_millis(500));

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-abort-finish".into(),
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
    driver.set_test_faults(faults.clone());

    let handle = driver.start(spec(&cwd)).await.expect("start");
    let mut events_rx = handle.into_events();
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

    // Background drainer keeps the 64-slot event channel empty at all times,
    // so a (buggy) flusher really can drain and exit — and counts FINAL
    // deliveries exactly. Barriers: a bind oneshot, a FINAL oneshot, and the
    // channel close after the pump/flusher wind down.
    let final_count = Arc::new(AtomicU32::new(0));
    let bound = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (final_tx, final_rx) = tokio::sync::oneshot::channel();
    let final_tx = std::sync::Mutex::new(Some(final_tx));
    let drainer_count = Arc::clone(&final_count);
    let drainer_bound = Arc::clone(&bound);
    let drainer = tokio::spawn(async move {
        while let Some(obs) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(payload) = &obs.body
                && let remuda_protocol::LifecyclePayload::Native(native) = payload.as_ref()
                && native.native_name == "transcript_bound"
            {
                drainer_bound.store(true, AtomicOrdering::SeqCst);
            }
            if let ObservationPayload::Usage(payload) = &obs.body
                && payload.scope_id == FINAL_MSG_ID
            {
                drainer_count.fetch_add(1, AtomicOrdering::SeqCst);
                if let Some(tx) = final_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }
        }
    });

    // Event barrier: wait for the bind, not an idle gap.
    let bind_deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !bound.load(AtomicOrdering::SeqCst) && std::time::Instant::now() < bind_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bound.load(AtomicOrdering::SeqCst), "transcript never bound");
    assert_eq!(
        final_count.load(AtomicOrdering::SeqCst),
        0,
        "the no-stop FINAL usage must not publish before close"
    );

    // Close crosses its shortened bound while the cooperative finish is
    // parked INSIDE the blocking operation.
    driver.close().await.expect("close");
    assert!(
        faults.wait_finish_entered(),
        "the cooperative finish never entered its barrier"
    );
    assert!(
        faults.wait_abort_residual(),
        "close returned but no timeout-driven abort happened"
    );

    // The abort guard must NOT have stopped the flusher while the cooperative
    // finish still owns the hydrator behind the shared in-flight flag. The
    // drainer keeps the channel empty, so a buggy guard that set stop here
    // lets the flusher drain and exit within the window; the fixed flusher
    // stays alive because stop is never set until the parked finish parks.
    assert!(
        !faults.wait_flusher_exit_for(Duration::from_millis(500)),
        "the flusher exited while the cooperative finish was still in flight"
    );
    assert_eq!(
        final_count.load(AtomicOrdering::SeqCst),
        0,
        "FINAL usage delivered before the parked finish was released"
    );

    // Release the detached cooperative finish: it parks the FINAL frames,
    // clears in-flight, the abort guard then retires and releases the single
    // flusher, which delivers them.
    faults.release();
    tokio::time::timeout(Duration::from_secs(30), final_rx)
        .await
        .expect("the aborted cooperative finish never delivered the FINAL usage")
        .expect("final oneshot closed without a frame");
    // r9 item 6: drain to channel close; two producers (finish + guard) ran,
    // the FINAL must still have been delivered exactly once.
    drainer.await.expect("event drainer panicked");
    assert_eq!(
        final_count.load(AtomicOrdering::SeqCst),
        1,
        "the aborted cooperative finish must deliver the FINAL usage exactly once"
    );
}

/// c-ctxusage r9 item 1: when the flusher has drained and is parked idle in
/// `notified().await`, a residual replay batch LARGER than the bounded queue
/// capacity must still be delivered — every usage frame, in full, exactly
/// once. Before the fix the producer filled the queue and blocked for
/// capacity BEFORE notifying, while the flusher could only be woken by that
/// never-arriving notify: circular wait, the residual usage (including the
/// no-stop FINAL) stranded on a parked thread.
///
/// The queue is shrunk to a handful of slots via the injected fault, so an
/// ordinary-sized replay exceeds it many times over; barriers only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_residual_batch_reaches_an_idle_flusher_once() {
    use remuda_driver::test_faults::TranscriptFaults;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

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
    fs::create_dir_all(&launch).unwrap();
    let transcript = launch.join("transcript.jsonl");
    // Each assistant record emits its own usage frame; 60 records dwarf the
    // shrunk queue and keep the producer parked across multiple refills.
    seed_transcript(&transcript, 60);

    let faults = TranscriptFaults::new();
    faults.arm_pump_gate();
    faults.arm_close_timeout(Duration::from_millis(500));
    // Four slots: the residual batch exceeds the capacity by orders of
    // magnitude, forcing notify-before-park wakeups to drain it.
    faults.set_pending_capacity(4);

    let driver = ClaudePtyDriver::new(ClaudePtyOptions {
        instance_id: None,
        media_stager: None,
        profile: profile(),
        launch_dir: launch.clone(),
        native_home: home,
        binary: BinarySource::Pinned(pin_binary(&claude).unwrap()),
        origin: LaunchOrigin::Human,
        session_name: "remuda-cp-oversized".into(),
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
    driver.set_test_faults(faults.clone());

    let handle = driver.start(spec(&cwd)).await.expect("start");
    let mut events_rx = handle.into_events();
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

    // Drain the channel and count EVERY usage frame by its native scope id —
    // the assertion is completeness + exactly-once, not a sample. A oneshot
    // fires once all 61 scopes (60 replay records + the no-stop FINAL) have
    // been seen, so the test waits on an acknowledgement rather than the
    // channel's unrelated process-teardown close.
    let counts = Arc::new(StdMutex::new(HashMap::<String, u32>::new()));
    let drainer_counts = Arc::clone(&counts);
    let (all_tx, all_rx) = tokio::sync::oneshot::channel();
    let all_tx = StdMutex::new(Some(all_tx));
    let drainer = tokio::spawn(async move {
        while let Some(obs) = events_rx.recv().await {
            if let ObservationPayload::Usage(payload) = &obs.body
                && !payload.scope_id.is_empty()
            {
                let mut seen = drainer_counts.lock().unwrap();
                *seen.entry(payload.scope_id.clone()).or_insert(0) += 1;
                if seen.len() == 61
                    && let Some(tx) = all_tx.lock().unwrap().take()
                {
                    let _ = tx.send(());
                }
            }
        }
    });

    // Barrier 1: the transcript bound and the first collect parked.
    assert!(
        faults.wait_pump_entered(),
        "the first blocking collect never entered its barrier"
    );
    // Barrier 2: the flusher drained (nothing queued yet) and parked idle —
    // it can now only be woken via the queue-integrated Notify.
    assert!(
        faults.wait_flusher_idle(),
        "the flusher never acknowledged its idle park"
    );
    // Close aborts the pump while its first collect is gated; the abort
    // residual runs (deterministically via the shortened bound) and waits on
    // the detached collect.
    driver.close().await.expect("close");
    assert!(
        faults.wait_abort_residual(),
        "close returned without the abort-path residual running"
    );

    // Release the replay: the oversized batch now flows through a four-slot
    // queue. With the circular wait it would never arrive and this
    // acknowledgement could not fire.
    faults.release();
    tokio::time::timeout(Duration::from_secs(30), all_rx)
        .await
        .expect("oversized residual batch stranded: not every usage arrived")
        .expect("completion oneshot closed without firing");
    // Flusher retirement means no producer remains: no frame can follow, so
    // the per-scope counts are final and assert exactly-once.
    assert!(
        faults.wait_flusher_exit_for(Duration::from_secs(30)),
        "the flusher never retired after the residual drain"
    );
    drainer.abort();

    let counts = counts.lock().unwrap();
    for index in 0..60u32 {
        let scope_id = format!("cp-msg-{index}");
        assert_eq!(
            counts.get(&scope_id).copied().unwrap_or(0),
            1,
            "replay usage {scope_id} must be delivered exactly once"
        );
    }
    assert_eq!(
        counts.get(FINAL_MSG_ID).copied().unwrap_or(0),
        1,
        "the no-stop FINAL usage must be delivered exactly once"
    );
}
