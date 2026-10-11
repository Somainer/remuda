//! c-ctxusage r8 item 1: the shell promoter's production shutdown sequence
//! deadlocked. The streaming flusher exits only once `flush_stop` AND
//! `residual_done` are set AND the queue is empty, but `residual_done` was
//! set only by `FinaliseGuard::disarm()`, which ran AFTER awaiting the
//! flusher. Every `close()` therefore waited the full 15 s
//! PROMOTER_FINALISE_TIMEOUT and was then aborted (stop_tree delayed with
//! it). These tests exercise the REAL production shutdown: a close must
//! finish promptly and the final frames must still arrive.
#![cfg(unix)]

use remuda_driver::shell_pty::{AgentLaunch, HookConfig, ShellPtyDriver, ShellPtyOptions};
use remuda_driver::{
    Delegation, Driver, DriverKind, LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_protocol::{AgentKind, InstanceSpec, ObservationPayload};
use remuda_testing::ensure_workspace_bin;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const FINAL_MSG_ID: &str = "msg_close_final_usage";
const SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000007".parse().unwrap(),
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

fn spec_for(cwd: &Path, driver: DriverKind) -> InstanceSpec {
    let mut spec: InstanceSpec = serde_json::from_str(include_str!(
        "../../remuda-driver/tests/fixtures/instance-spec.json"
    ))
    .unwrap();
    spec.driver = driver;
    spec.kind = AgentKind::Claude;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("haiku".into());
    spec
}

fn final_record() -> String {
    let record = serde_json::json!({
        "type": "assistant",
        "uuid": "close-final-record",
        "timestamp": "2026-10-09T00:02:00.000Z",
        "isSidechain": false,
        "message": {
            "id": FINAL_MSG_ID,
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": null,
            "content": [{"type": "text", "text": "FINAL_CLOSE_TURN"}],
            "usage": {
                "input_tokens": 5_555,
                "cache_read_input_tokens": 8_888,
                "cache_creation_input_tokens": 44,
                "output_tokens": 66,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 44,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        }
    });
    format!("{record}\n")
}

fn filler(seed: &str, index: u32) -> String {
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
            "content": [{"type": "text", "text": "x"}],
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

fn seed_transcript(path: &Path, pairs: u32) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap()).expect("project dir");
    let mut file = std::fs::File::create(path).expect("create transcript");
    for index in 0..pairs {
        file.write_all(filler("fill", index).as_bytes()).unwrap();
    }
    file.write_all(final_record().as_bytes()).unwrap();
    file.sync_all().unwrap();
}

/// An UNBOUND promoted session (promoter running, no agent ever binds): close
/// must still drain the flusher and return immediately. Before the fix the
/// flusher waited for a `residual_done` nobody set on this path and close
/// hung for the full 15 s finalise timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unbound_shell_close_finishes_well_under_one_second() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    // A non-interactive shell WITH the promoter switched on: it never binds
    // an agent (the unbound case), and unlike a login shell it dies on the
    // stop ladder's first SIGINT, so the measured time is the poller/flush
    // handshake — not process teardown.
    let mut options = ShellPtyOptions::login(workspace.clone());
    options.promote = true;
    options.emulator = false;
    options.shell = std::path::PathBuf::from("/bin/sh");
    // build_command uses args[0] as the program (and skips the login `-l`).
    options.args = vec!["/bin/sh".into(), "-c".into(), "read _unused".into()];
    let driver = ShellPtyDriver::new(options);

    let _handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    // Let the promoter complete at least one poll (barrier-free: close itself
    // is what must not block; a couple of runtime ticks is enough).
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = Instant::now();
    driver.close().await.expect("close");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "unbound close took {:?}; the flusher/residual handshake deadlocked \
         the full finalise timeout",
        started.elapsed()
    );
}

/// A BOUND promoted session closes cooperatively: close finishes promptly and
/// the no-stop FINAL usage is delivered by the drain. Before the fix the
/// residual-vs-flusher ordering aborted every close at 15 s even though the
/// cooperative finalise had already parked the frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_shell_close_finishes_well_under_one_second_and_delivers_final_usage() {
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

    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let final_count = Arc::new(AtomicUsize::new(0));
    let bound = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drainer_count = Arc::clone(&final_count);
    let drainer_bound = Arc::clone(&bound);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
            {
                drainer_bound.store(true, Ordering::SeqCst);
            }
            if let ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id == FINAL_MSG_ID
            {
                drainer_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    // Wait for readiness and the bind (event barriers, not idle gaps).
    let ready_deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < ready_deadline {
        if Driver::wait_control(&driver).await.is_ok() {
            break;
        }
    }
    assert!(
        Driver::wait_control(&driver).await.is_ok(),
        "composer never became ready"
    );
    let bind_deadline = Instant::now() + Duration::from_secs(20);
    while !bound.load(Ordering::SeqCst) && Instant::now() < bind_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        bound.load(Ordering::SeqCst),
        "the seeded replay never bound"
    );
    assert_eq!(
        final_count.load(Ordering::SeqCst),
        0,
        "the no-stop FINAL usage must not publish before close"
    );

    let started = Instant::now();
    driver.close().await.expect("close");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "bound cooperative close took {:?}; expected prompt flusher drain",
        started.elapsed()
    );

    // The final usage arrives from the cooperative drain exactly once.
    let delivered_deadline = Instant::now() + Duration::from_secs(10);
    while final_count.load(Ordering::SeqCst) == 0 && Instant::now() < delivered_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drainer.abort();
    assert_eq!(
        final_count.load(Ordering::SeqCst),
        1,
        "cooperative close must deliver the FINAL no-stop usage exactly once"
    );
}

/// c-ctxusage r9 item 2: close aborts the promoter while the FIRST blocking
/// collect is merely QUEUED — registered in the producer gate but parked
/// BEFORE it claims the hydrator. The old loose atomics let the abort guard
/// read "no producer, empty slot" in that gap, declare completion on a ~120 s
/// budget, and let the flusher exit before the queued collect (and the final
/// finish) parked their frames. The unified gate makes the queued closure
/// Active before it runs; the terminal finish waits on actual retirement. The
/// FINAL no-stop usage must still be delivered exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_before_a_queued_collect_claims_still_delivers_final_usage_once() {
    use remuda_driver::test_faults::TranscriptFaults;

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

    // Park the first blocking collect the moment its closure runs, BEFORE it
    // claims the hydrator; shorten close's bound so the abort is deterministic.
    let faults = TranscriptFaults::new();
    faults.arm_queued_gate();
    faults.arm_close_timeout(Duration::from_millis(500));
    driver.set_test_faults(faults.clone());

    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let final_count = Arc::new(AtomicUsize::new(0));
    let bound = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drainer_count = Arc::clone(&final_count);
    let drainer_bound = Arc::clone(&bound);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
            {
                drainer_bound.store(true, Ordering::SeqCst);
            }
            if let ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id == FINAL_MSG_ID
            {
                drainer_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    let ready_deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < ready_deadline {
        if Driver::wait_control(&driver).await.is_ok() {
            break;
        }
    }
    let bind_deadline = Instant::now() + Duration::from_secs(20);
    while !bound.load(Ordering::SeqCst) && Instant::now() < bind_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        bound.load(Ordering::SeqCst),
        "the seeded replay never bound"
    );

    // Barrier: the first collect closure is queued and parked BEFORE its
    // claim — the producer is registered but the hydrator is unclaimed.
    assert!(
        faults.wait_queued_entered(),
        "the queued blocking collect never reached its pre-claim barrier"
    );

    // Close crosses its shortened bound and aborts the promoter.
    driver.close().await.expect("close");
    assert!(
        faults.wait_abort_residual(),
        "close returned without the abort residual running"
    );

    // The terminal finish is YIELDING to the parked collector: nothing may
    // be finished, the flusher must not retire while the queued producer is
    // active.
    assert!(
        !faults.wait_flusher_exit_for(Duration::from_millis(500)),
        "the flusher retired while the queued collect was still registered"
    );
    assert_eq!(
        final_count.load(Ordering::SeqCst),
        0,
        "FINAL usage delivered while the queued collect was parked"
    );

    // Release: the collector claims, maps the replay and restores; the
    // waiting terminal finish then claims, finishes the buffered run and
    // retires; the flusher delivers the FINAL usage exactly once.
    faults.release();
    let delivered_deadline = Instant::now() + Duration::from_secs(30);
    while final_count.load(Ordering::SeqCst) < 1 && Instant::now() < delivered_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        final_count.load(Ordering::SeqCst),
        1,
        "the queued collect + abort finish must deliver the FINAL usage exactly once"
    );
    // Let the flusher wind down, then prove exactly-once even though both a
    // collector and the abort terminal finish ran.
    assert!(
        faults.wait_flusher_exit_for(Duration::from_secs(30)),
        "the flusher never retired after the queued collect drained"
    );
    drainer.abort();
    assert_eq!(
        final_count.load(Ordering::SeqCst),
        1,
        "the queued collect + abort finish must deliver the FINAL usage exactly once"
    );
}
