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
    remuda_protocol::DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "msg-barrier-1".into(),
    }))
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

    let barrier = remuda_driver::claude_print::test_barrier::arm();
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
