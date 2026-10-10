//! ma-sdk-state r4 item 1, dual projection: with the REAL sdk driver forced
//! into the write-ack/result race (the driver's test-stub write-commit
//! barrier), the emitted stream starts the turn before its result, and the
//! ordered observations project
//!
//! * working → idle through the NODE's `engine_turn_activity`, and
//! * activity working then idle on the HUB durable row when appended through
//!   the real journal fold (`Store::append_journal`).
//!
//! Pre-fix the result was mapped against an empty turn book and projected
//! idle before the start; the start then stuck the row at `working` forever.

use remuda_driver::claude_sdk::{ClaudeSdkDriver, ClaudeSdkOptions};
use remuda_driver::{
    BinaryPin, BinarySource, Driver, ProviderHealth, ProviderKind, ProviderProfile, SecretRef,
};
use remuda_hub::store_test_support;
use remuda_node::signal::engine_turn_activity;
use remuda_protocol::{
    Activity, ContentBlock, Digest, DispatchState, DriverKind, InputOrigin, InstanceSpec,
    JournalEvent, LifecyclePayload, Observation, ObservationPayload, PromptInput, PromptMode,
    TextBlock,
};
use remuda_testing::{ScriptKind, ensure_workspace_bin, script_path};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

fn ensure_fake_claude() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let path = ensure_workspace_bin("fake-claude");
        assert!(path.is_file(), "missing {}", path.display());
        path
    })
    .clone()
}

fn pin_fake() -> BinaryPin {
    let path = ensure_fake_claude().canonicalize().unwrap();
    BinaryPin {
        abs_path: path.to_string_lossy().into_owned(),
        version: "fake-claude".into(),
        sha256: Digest::try_from(format!("sha256:{:0>64}", "a")).unwrap(),
    }
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: "https://gateway.example".into(),
        delegation: remuda_driver::Delegation::None,
        secret_ref: Some(SecretRef::parse("env:REMUDA_CLAUDE_SDK_SECRET").unwrap()),
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn spec(tmp: &std::path::Path) -> InstanceSpec {
    let mut spec: InstanceSpec = serde_json::from_str(include_str!(
        "../../remuda-driver/tests/fixtures/instance-spec.json"
    ))
    .unwrap();
    spec.driver = DriverKind::ClaudeSdk;
    spec.cwd = tmp.canonicalize().unwrap().to_string_lossy().into_owned();
    spec
}

fn prompt(text: &str) -> remuda_protocol::DriverInput {
    prompt_id(text, "msg-barrier-1")
}

fn prompt_id(text: &str, client_message_id: &str) -> remuda_protocol::DriverInput {
    remuda_protocol::DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: client_message_id.into(),
    }))
}

/// Launch the REAL sdk driver against a fake-claude playing `kind`.
async fn launch(
    kind: ScriptKind,
) -> (tempfile::TempDir, ClaudeSdkDriver, remuda_driver::RunHandle) {
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let mut extra = BTreeMap::new();
    extra.insert(
        "FAKE_CLAUDE_SCRIPT".into(),
        script_path(kind).to_string_lossy().into_owned(),
    );
    let mut options =
        ClaudeSdkOptions::new(profile(), launch, home, BinarySource::Pinned(pin_fake()));
    options.origin = InputOrigin::Human;
    options.extra_env = extra;
    options.handshake_timeout = Duration::from_secs(5);
    options.close_timeout = Duration::from_secs(3);
    let driver = ClaudeSdkDriver::new(options);
    let handle = driver.start(spec(tmp.path())).await.expect("start");
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);
    (tmp, driver, handle)
}

fn is_turn(obs: &Observation, name: &str) -> bool {
    matches!(&obs.body,
        ObservationPayload::Lifecycle(p) if matches!(p.as_ref(),
            LifecyclePayload::Native(n) if n.topic == remuda_protocol::LifecycleTopic::Turn
                && n.native_name == name))
}

async fn collect_until_result(handle: &mut remuda_driver::RunHandle) -> Vec<Observation> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut out = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(obs) = tokio::time::timeout(remaining, handle.recv())
            .await
            .expect("timed out waiting for the turn result")
        else {
            panic!("stream closed before the result: {out:?}");
        };
        let is_result = is_turn(&obs, "result");
        out.push(obs);
        if is_result {
            break;
        }
    }
    out
}

#[tokio::test]
async fn write_window_race_publishes_start_first_and_both_projections_settle_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let mut extra = BTreeMap::new();
    extra.insert(
        "FAKE_CLAUDE_SCRIPT".into(),
        script_path(ScriptKind::Ok).to_string_lossy().into_owned(),
    );
    let mut options =
        ClaudeSdkOptions::new(profile(), launch, home, BinarySource::Pinned(pin_fake()));
    options.origin = InputOrigin::Human;
    options.extra_env = extra;
    options.handshake_timeout = Duration::from_secs(5);
    options.close_timeout = Duration::from_secs(3);
    let driver = ClaudeSdkDriver::new(options);

    let mut handle = driver.start(spec(tmp.path())).await.expect("start");
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);

    let barrier = driver.arm_write_commit_barrier();
    let send_task = tokio::spawn(async move {
        let result = driver.send(prompt("race prompt")).await;
        (driver, result)
    });
    barrier
        .wait_until_result_parked(Duration::from_secs(5))
        .await;
    barrier.release();
    let (driver, result) = send_task.await.expect("send task");
    result.expect("the prompt write acked");

    let observations = collect_until_result(&mut handle).await;
    driver.close().await.expect("close");

    // ── Stream order: the turn's start precedes that turn's result ──────────
    let start_pos = observations
        .iter()
        .position(|o| is_turn(o, "turn_started"))
        .expect("turn_started emitted");
    let result_pos = observations
        .iter()
        .position(|o| is_turn(o, "result"))
        .expect("result emitted");
    assert!(
        start_pos < result_pos,
        "start {start_pos} must precede result {result_pos}"
    );

    // ── Node projection: working at the start, idle exactly at the result ───
    let engine: Vec<Activity> = observations
        .iter()
        .filter_map(engine_turn_activity)
        .collect();
    assert_eq!(
        engine.first(),
        Some(&Activity::Working),
        "the Node engine goes working at the start"
    );
    assert_eq!(
        engine.last(),
        Some(&Activity::Idle),
        "the Node engine settles idle at the result"
    );

    // ── Hub projection: append the real observations in emitted order to a
    // real Store holding the instance, and watch the durable row go working
    // then idle (never idle-stuck-before-the-start).
    let hub_dir = tempfile::tempdir().unwrap();
    let (store, host_id) = store_test_support::open_with_host(hub_dir.path(), "r4-barrier")
        .await
        .expect("hub store + host");
    let instance_id = observations
        .first()
        .expect("at least one observation")
        .instance_id
        .as_id()
        .to_string();
    store
        .ensure_instance(host_id.clone(), instance_id.clone())
        .await
        .expect("ensure instance");

    let mut saw_working = false;
    for (index, mut observation) in observations.into_iter().enumerate() {
        observation.instance_id =
            remuda_protocol::InstanceId::try_from(instance_id.clone()).unwrap();
        let event = serde_json::to_value(JournalEvent::Instance(Box::new(observation)))
            .expect("observation serializes as a journal event");
        store
            .append_journal(host_id.clone(), instance_id.clone(), None, event)
            .await
            .expect("append");
        let row = store
            .get_instance(instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        if row.activity == "working" {
            saw_working = true;
        }
        if index < result_pos {
            assert_ne!(
                row.activity, "idle",
                "before the result the durable row must not be idle"
            );
        }
    }
    assert!(saw_working, "the durable row passed through working");
    let row = store
        .get_instance(instance_id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(
        row.activity, "idle",
        "the settled result idles the durable row, final state {row:?}"
    );
    assert_eq!(
        row.lifecycle, "running",
        "a turn result never ends the sdk process"
    );
    store.close().await;
}

/// Collect until exactly `n` turn `result` observations have been seen.
async fn collect_n_results(handle: &mut remuda_driver::RunHandle, n: usize) -> Vec<Observation> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    let mut results = 0usize;
    while results < n {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(obs) = tokio::time::timeout(remaining, handle.recv())
            .await
            .expect("timed out waiting for turn results")
        else {
            panic!("stream closed after {results}/{n} results: {out:?}");
        };
        if is_turn(&obs, "result") {
            results += 1;
        }
        out.push(obs);
    }
    out
}

/// Fold the emitted observations through a real Hub Store and assert the
/// durable row passes through working, ends idle, and is never idle before the
/// first start. Returns the final activity.
async fn assert_store_working_then_idle(observations: &[Observation], label: &str) {
    let hub_dir = tempfile::tempdir().unwrap();
    let (store, host_id) = store_test_support::open_with_host(hub_dir.path(), label)
        .await
        .expect("hub store + host");
    let instance_id = observations
        .first()
        .expect("at least one observation")
        .instance_id
        .as_id()
        .to_string();
    store
        .ensure_instance(host_id.clone(), instance_id.clone())
        .await
        .expect("ensure instance");

    let first_start = observations
        .iter()
        .position(|o| is_turn(o, "turn_started"))
        .expect("a turn_started exists");
    let mut saw_working = false;
    for (index, mut observation) in observations.iter().cloned().enumerate() {
        observation.instance_id =
            remuda_protocol::InstanceId::try_from(instance_id.clone()).unwrap();
        let event = serde_json::to_value(JournalEvent::Instance(Box::new(observation)))
            .expect("observation serializes as a journal event");
        store
            .append_journal(host_id.clone(), instance_id.clone(), None, event)
            .await
            .expect("append");
        let row = store
            .get_instance(instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        if row.activity == "working" {
            saw_working = true;
        }
        if index < first_start {
            assert_ne!(
                row.activity, "idle",
                "[{label}] idle before the first start"
            );
        }
    }
    assert!(
        saw_working,
        "[{label}] the durable row passed through working"
    );
    let row = store
        .get_instance(instance_id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(
        row.activity, "idle",
        "[{label}] final durable activity is idle"
    );
    assert_eq!(
        row.lifecycle, "running",
        "[{label}] results never end the sdk process"
    );
    store.close().await;
}

/// Variant: the send commits while the worker is parked at the turn's Reserve
/// ticket but BEFORE the child's result frame is queued (the common,
/// non-racing path). The worker still publishes turn_started before the result
/// it maps afterwards — start-first must not depend on the result winning the
/// race.
#[tokio::test]
async fn send_commits_before_the_result_is_mapped_still_publishes_start_first() {
    let (_tmp, driver, mut handle) = launch(ScriptKind::Ok).await;

    let barrier = driver.arm_write_commit_barrier();
    let send_task = tokio::spawn(async move {
        let result = driver.send(prompt("commit-first prompt")).await;
        (driver, result)
    });
    // Release as soon as the worker holds the ticket — do not wait for the
    // child's result to be enqueued.
    barrier
        .wait_until_reserve_parked(Duration::from_secs(5))
        .await;
    barrier.release();
    let (driver, result) = send_task.await.expect("send task");
    result.expect("the prompt write acked");

    let observations = collect_until_result(&mut handle).await;
    driver.close().await.expect("close");

    let start_pos = observations
        .iter()
        .position(|o| is_turn(o, "turn_started"))
        .expect("turn_started emitted");
    let result_pos = observations
        .iter()
        .position(|o| is_turn(o, "result"))
        .expect("result emitted");
    assert!(
        start_pos < result_pos,
        "start {start_pos} precedes result {result_pos}"
    );

    let engine: Vec<Activity> = observations
        .iter()
        .filter_map(engine_turn_activity)
        .collect();
    assert_eq!(engine.first(), Some(&Activity::Working));
    assert_eq!(engine.last(), Some(&Activity::Idle));
    assert_store_working_then_idle(&observations, "r5-commit-first").await;
}

/// Two-turn variant (turn N result vs turn N+1 reservation): across two
/// prompts on the long-lived sdk child, EACH turn's start precedes that same
/// turn's result and the two results stay attributed per turn (result_index 0
/// then 1). Turn A is held behind its ticket; the moment A commits, B is
/// sent so its reservation registers adjacent to A's result — the in-queue
/// ticket makes start-before-result structural for both turns.
#[tokio::test]
async fn two_turns_publish_each_start_before_its_own_result_with_per_turn_attribution() {
    let (_tmp, driver, mut handle) = launch(ScriptKind::TwoTurn).await;

    let barrier = driver.arm_write_commit_barrier();
    let send_a = tokio::spawn(async move {
        let result = driver.send(prompt_id("first", "msg-barrier-a")).await;
        (driver, result)
    });
    // A's worker is parked at its ticket; commit it and immediately register B
    // so B's reservation is in the publication queue next to A's result.
    barrier
        .wait_until_reserve_parked(Duration::from_secs(5))
        .await;
    barrier.release();
    let (driver, result_a) = send_a.await.expect("send A");
    result_a.expect("first send");
    driver
        .send(prompt_id("second", "msg-barrier-b"))
        .await
        .expect("second send");

    let observations = collect_n_results(&mut handle, 2).await;
    driver.close().await.expect("close");

    let starts: Vec<usize> = observations
        .iter()
        .enumerate()
        .filter_map(|(i, o)| is_turn(o, "turn_started").then_some(i))
        .collect();
    let results: Vec<usize> = observations
        .iter()
        .enumerate()
        .filter_map(|(i, o)| is_turn(o, "result").then_some(i))
        .collect();
    assert_eq!(starts.len(), 2, "two turn_started: {starts:?}");
    assert_eq!(results.len(), 2, "two results: {results:?}");
    // Each turn's start precedes that same turn's result, in turn order.
    assert!(starts[0] < results[0], "start A before result A");
    assert!(starts[1] < results[1], "start B before result B");

    // Per-turn attribution: the results arrive with process-global indices 0
    // then 1, in that order.
    let indices: Vec<i64> = observations
        .iter()
        .filter_map(|o| match &o.body {
            ObservationPayload::Lifecycle(p) => match p.as_ref() {
                LifecyclePayload::Native(n) if n.native_name == "result" => n
                    .related_ids
                    .get("resultIndex")
                    .and_then(|v| v.parse().ok()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(indices, vec![0, 1], "results attributed per turn, in order");

    // The engine is working at the first start, idle only at the final result.
    let engine: Vec<Activity> = observations
        .iter()
        .filter_map(engine_turn_activity)
        .collect();
    assert_eq!(engine.first(), Some(&Activity::Working), "starts working");
    assert_eq!(engine.last(), Some(&Activity::Idle), "settles idle last");
    assert_store_working_then_idle(&observations, "r5-two-turn").await;
}

/// ma-sdk-state r5 item 6 (5a): A's result is BUFFERED while turn B is
/// published (the item-1 two-gate barrier puts B's reservation in the queue
/// before the worker maps A's result). The per-turn attribution must hold on
/// BOTH projections: when A's result is folded with B outstanding,
/// engine_turn_activity yields no Idle and the Hub durable row stays working;
/// only B's own settled result idles. Assertions are made at the exact point
/// A's result (resultIndex 0) is observed, before B's result (resultIndex 1).
#[tokio::test]
async fn buffered_turn_a_result_keeps_the_row_working_until_turn_b_settles() {
    let (_tmp, driver, mut handle) = launch(ScriptKind::TwoTurn).await;

    let barrier = driver.arm_write_commit_barrier();
    let send_both = tokio::spawn(async move {
        driver.send(prompt_id("first", "msg-5a-a")).await?;
        driver.send(prompt_id("second", "msg-5a-b")).await?;
        Ok::<ClaudeSdkDriver, remuda_driver::DriverError>(driver)
    });
    barrier
        .wait_until_reserve_parked(Duration::from_secs(5))
        .await;
    // Commit A (start_A emitted) but hold the worker at the ticket while B
    // registers, then let the worker map the buffered A result with B's book
    // already open.
    barrier.release_send();
    barrier
        .wait_until_reserves_enqueued(2, Duration::from_secs(5))
        .await;
    barrier.resume_worker();

    // A real Hub projection, fed the real observations in emission order.
    let hub_dir = tempfile::tempdir().unwrap();
    let (store, host_id) = store_test_support::open_with_host(hub_dir.path(), "r5-5a-buffered")
        .await
        .expect("hub store + host");

    let mut instance_id: Option<String> = None;
    let mut engine: Option<Activity> = None;
    let mut a_seen = false;
    let mut activity_at_a: Option<String> = None;
    let mut engine_at_a: Option<Activity> = None;

    // Drain until A's result (resultIndex 0) has been folded.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !a_seen {
        let obs = tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            handle.recv(),
        )
        .await
        .expect("timeout waiting for turn A result")
        .expect("stream closed");
        if instance_id.is_none() {
            let id = obs.instance_id.as_id().to_string();
            store
                .ensure_instance(host_id.clone(), id.clone())
                .await
                .expect("ensure");
            instance_id = Some(id);
        }
        let id = instance_id.clone().unwrap();
        let mut to_append = obs.clone();
        to_append.instance_id = remuda_protocol::InstanceId::try_from(id.clone()).unwrap();
        let event =
            serde_json::to_value(JournalEvent::Instance(Box::new(to_append))).expect("event");
        store
            .append_journal(host_id.clone(), id.clone(), None, event)
            .await
            .expect("append");
        if let Some(activity) = engine_turn_activity(&obs) {
            engine = Some(activity);
        }
        let is_a_result = is_turn(&obs, "result") && result_index_of(&obs).as_deref() == Some("0");
        if is_a_result {
            a_seen = true;
            let row = store.get_instance(id).await.expect("get").expect("row");
            activity_at_a = Some(row.activity.clone());
            engine_at_a = engine;
        }
    }

    assert_eq!(
        engine_at_a,
        Some(Activity::Working),
        "the Node engine must not idle on buffered turn A while B is outstanding"
    );
    assert_eq!(
        activity_at_a.as_deref(),
        Some("working"),
        "the Hub row stays working through buffered turn A's result"
    );

    // Drain B's result and confirm the final settle.
    let mut final_engine = engine_at_a;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let obs = match tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            handle.recv(),
        )
        .await
        {
            Ok(Some(obs)) => obs,
            _ => break,
        };
        if let Some(activity) = engine_turn_activity(&obs) {
            final_engine = Some(activity);
        }
        let id = instance_id.clone().unwrap();
        let mut to_append = obs.clone();
        to_append.instance_id = remuda_protocol::InstanceId::try_from(id.clone()).unwrap();
        let event =
            serde_json::to_value(JournalEvent::Instance(Box::new(to_append))).expect("event");
        store
            .append_journal(host_id.clone(), id.clone(), None, event)
            .await
            .expect("append");
        if is_turn(&obs, "result") && result_index_of(&obs).as_deref() == Some("1") {
            break;
        }
    }
    assert_eq!(
        final_engine,
        Some(Activity::Idle),
        "turn B settles the engine idle"
    );
    let row = store
        .get_instance(instance_id.unwrap())
        .await
        .expect("get")
        .expect("row");
    assert_eq!(
        row.activity, "idle",
        "turn B's settled result idles the Hub row"
    );
    assert_eq!(row.lifecycle, "running", "the sdk process is still alive");
    store.close().await;

    let driver = send_both.await.expect("send task").expect("both sends ack");
    driver.close().await.expect("close");
}

fn result_index_of(obs: &Observation) -> Option<String> {
    match &obs.body {
        ObservationPayload::Lifecycle(p) => match p.as_ref() {
            LifecyclePayload::Native(n) if n.native_name == "result" => {
                n.related_ids.get("resultIndex").cloned()
            }
            _ => None,
        },
        _ => None,
    }
}
