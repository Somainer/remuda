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

    // A plain shell's wait_control() returns Ok immediately (there is no
    // composer to gate on), so it cannot prove the promoter exists. Use the
    // promoter-started acknowledgement instead: it fires only once the
    // promoter task, flusher and gate handshake are live (r10 item 5). It
    // must NOT fire before start.
    use remuda_driver::test_faults::TranscriptFaults;
    let faults = TranscriptFaults::new();
    assert!(
        !faults.wait_promoter_started_for(Duration::from_millis(300)),
        "the promoter-started ack fired before start: it proves nothing"
    );
    driver.set_test_faults(faults.clone());

    let _handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    // Acknowledgement barrier: the promoter task and its flusher/residual
    // handshake exist, which is what close must not deadlock against.
    assert!(
        faults.wait_promoter_started_for(Duration::from_secs(25)),
        "the promoter task/flusher handshake never came up"
    );

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
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer_count = Arc::clone(&final_count);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id == FINAL_MSG_ID
            {
                drainer_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    // Event barrier: the transcript_bound event, not an idle gap.
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");
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

    // Exactly-once counting waits for RECEIVER COMPLETION (the event channel
    // closing after flusher retirement), under a timeout — aborting the
    // drainer here could leave a duplicate still buffered in the channel and
    // uncounted (r10 item 5). The long-lived per-session workers are the last
    // sender clones; stop them (test seam) to observe channel closure.
    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed after cooperative close")
        .expect("event drainer panicked");
    assert_eq!(
        final_count.load(Ordering::SeqCst),
        1,
        "cooperative close must deliver the FINAL no-stop usage exactly once"
    );
}

/// c-ctxusage r10 item 1 (was the r9 item 2 regression): close aborts the
/// promoter while the cooperative TERMINAL finalise is registered and its
/// blocking closure is parked BEFORE it claims the hydrator. The abort
/// residual then registers a SECOND terminal. The r9 gate made both see
/// "another terminal" and wait on each other forever. Ticket order elects the
/// earlier (cooperative) terminal the sole owner; the residual no-ops after
/// retirement. The FINAL no-stop usage must still be delivered exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_before_a_queued_terminal_claims_still_delivers_final_usage_once() {
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

    // Park the first registered TERMINAL finalise the moment its blocking
    // closure starts, BEFORE it claims the hydrator; shorten close's bound so
    // the abort (and its second terminal) is deterministic.
    let faults = TranscriptFaults::new();
    faults.arm_terminal_gate();
    faults.arm_close_timeout(Duration::from_millis(500));
    driver.set_test_faults(faults.clone());

    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let final_count = Arc::new(AtomicUsize::new(0));
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer_count = Arc::clone(&final_count);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id == FINAL_MSG_ID
            {
                drainer_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    // Acknowledgement barriers only: the promoter handshake is live, then the
    // seeded replay binds (an event, not an idle gap).
    assert!(
        faults.wait_promoter_started_for(Duration::from_secs(25)),
        "the promoter task/flusher handshake never came up"
    );
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");

    // Close crosses its shortened bound while the cooperative terminal
    // finalise is merely REGISTERED+queued, and aborts the promoter.
    driver.close().await.expect("close");
    assert!(
        faults.wait_abort_residual(),
        "close returned without the abort residual running"
    );
    // Barrier: the cooperative terminal closure reached its pre-claim park
    // (registered, hydrator unclaimed).
    assert!(
        faults.wait_terminal_entered(),
        "the cooperative terminal finalise never reached its pre-claim barrier"
    );
    // Barrier: the abort residual's SECOND terminal registered and parked in
    // its claim behind the elected cooperative owner. No elapsed-time
    // inference: without this ack the test could release before the second
    // terminal ever existed and still pass (c-ctxusage r11 item 3).
    assert!(
        faults.wait_abort_terminal_waiting_for(Duration::from_secs(30)),
        "the abort terminal never registered/waited behind the elected owner"
    );

    // Neither terminal may finish while the elected one is parked: the
    // flusher must not retire and no FINAL usage is delivered. The state is
    // already established by the two barriers above, so these are immediate
    // observations, not a settling wait.
    assert!(
        !faults.flusher_exited(),
        "the flusher retired while the terminal owner was still parked"
    );
    assert_eq!(
        final_count.load(Ordering::SeqCst),
        0,
        "FINAL usage delivered while the terminal owner was parked"
    );

    // Release: the elected cooperative terminal claims, finishes the
    // buffered run and retires; the residual terminal wakes retired and
    // no-ops. The flusher delivers the FINAL usage exactly once and retires.
    faults.release();
    assert!(
        faults.wait_flusher_exit_for(Duration::from_secs(30)),
        "the flusher never retired after the terminal finish drained"
    );
    // Await RECEIVER COMPLETION (channel close) under a timeout before
    // counting: aborting here could leave a duplicate still buffered in the
    // channel and uncounted (r10 item 5). The long-lived per-session workers
    // are the last sender clones after the promoter/waiter shut down.
    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed after flusher retirement")
        .expect("event drainer panicked");
    assert_eq!(
        final_count.load(Ordering::SeqCst),
        1,
        "cooperative terminal + abort residual must deliver the FINAL usage exactly once"
    );
}

const TERMINATION_MSG_ID: &str = "msg_termination_signal";

/// c-ctxusage r15 item 7: a promoted native writer flushes one final
/// transcript record WHILE HANDLING the close signal (the fake harness
/// appends it on SIGINT, then exits). Close must stop and verify the native
/// writer first, keeping the hydrator and sender live, drain that
/// termination-triggered append between the stop-ladder rungs, and only then
/// retire the hydrator. Before the fix close retired the promoter before the
/// stop ladder ran, so the SIGINT-flushed record landed after the last tail
/// read and was lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promoted_close_drains_the_writers_termination_append_before_hydrator_retires() {
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
    // The native writer's termination flush, modelled deterministically.
    options
        .extra_env
        .insert("FAKE_HARNESS_APPEND_ON_SIGNAL".into(), "1".into());
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
    let term_count = Arc::new(AtomicUsize::new(0));
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer_count = Arc::clone(&term_count);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id == TERMINATION_MSG_ID
            {
                drainer_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");
    assert_eq!(
        term_count.load(Ordering::SeqCst),
        0,
        "the termination record must not exist before the close signal"
    );

    let started = Instant::now();
    // Bounded wait (c-ctxusage r16 item 2): a wedged carrier must hang the
    // TEST as a failure, not forever. The close itself already bounds every
    // carrier dial/handshake; this outer bound guards the whole promoted
    // close path.
    tokio::time::timeout(Duration::from_secs(15), driver.close())
        .await
        .expect("promoted close hung past the bounded wait")
        .expect("promoted close");
    // The ladder's polite rung alone has a 2 s settle bound plus the bounded
    // live drain; a prompt writer exits within it, so the whole close stays
    // well under the escalated-rung and promoter timeouts.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "promoted draining close took {:?}",
        started.elapsed()
    );

    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed after close")
        .expect("event drainer panicked");
    assert_eq!(
        term_count.load(Ordering::SeqCst),
        1,
        "the writer's SIGINT-flushed termination usage must be drained exactly once, \
         before the hydrator retired"
    );

    // The record really was the writer's append (present on disk), proving the
    // drain read a record written after the close signal.
    let body = std::fs::read_to_string(&transcript).expect("transcript");
    assert!(
        body.contains(TERMINATION_MSG_ID),
        "the fake writer appended the termination record to the transcript"
    );
}

/// P1 (c-ctxusage r16 item 5): a promoted close whose stop ladder cannot
/// verify the writer's process group gone must NOT retire the hydrator or
/// drop close ownership — it returns a RETRYABLE error, keeps transcript
/// monitoring live (the SIGINT-flushed termination append is still
/// delivered), and a subsequent close completes after the group is really
/// gone. The first attempt's failure is injected deterministically; the
/// second attempt runs the real verification against the now-dead group.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_incomplete_promoted_close_stays_live_and_a_retry_completes() {
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
    // The native writer appends one final record on SIGINT, then exits.
    options
        .extra_env
        .insert("FAKE_HARNESS_APPEND_ON_SIGNAL".into(), "1".into());
    options.hooks = Some(HookConfig {
        instance_dir,
        relay_binary: relay_bin(),
        tui: remuda_driver::TuiMode::Default,
    });
    let driver = ShellPtyDriver::new(options);

    let faults = TranscriptFaults::new();
    // The FIRST promoted close reports the writer still live after the ladder.
    faults.arm_promoted_stop_incomplete(1);
    driver.set_test_faults(faults.clone());

    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let term_count = Arc::new(AtomicUsize::new(0));
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer_count = Arc::clone(&term_count);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id == TERMINATION_MSG_ID
            {
                drainer_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");

    // Attempt 1: the ladder really runs (the writer appends and exits), but
    // verification is injected as incomplete. The close must surface the
    // RETRYABLE WriterStillLive error rather than reporting success.
    let first = tokio::time::timeout(Duration::from_secs(15), driver.close())
        .await
        .expect("the first close hung past its bound");
    assert!(
        matches!(first, Err(remuda_driver::DriverError::WriterStillLive(_))),
        "an unverified promoted close is retryable, got {first:?}"
    );

    // The hydrator/poller/sender were never retired: the writer's SIGINT
    // append is still delivered through the live channel.
    let deadline = Instant::now() + Duration::from_secs(15);
    while term_count.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        term_count.load(Ordering::SeqCst),
        1,
        "the termination append is delivered while the close is incomplete"
    );

    // Attempt 2: the group is really gone, so the verification succeeds and
    // the teardown completes.
    tokio::time::timeout(Duration::from_secs(15), driver.close())
        .await
        .expect("the retried close hung past its bound")
        .expect("the retried close completes once the writer is verified gone");

    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed after the retried close")
        .expect("event drainer panicked");
    assert_eq!(
        term_count.load(Ordering::SeqCst),
        1,
        "the termination usage is delivered exactly once across both attempts"
    );
}

/// Build the promoted-claude fake-harness layout shared by the P1 race tests
/// (c-ctxusage r16 item 6).
fn p1_promoted_layout() -> (tempfile::TempDir, PathBuf, PathBuf, ShellPtyDriver) {
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
    options
        .extra_env
        .insert("FAKE_HARNESS_APPEND_ON_SIGNAL".into(), "1".into());
    options.hooks = Some(HookConfig {
        instance_dir,
        relay_binary: relay_bin(),
        tui: remuda_driver::TuiMode::Default,
    });
    let driver = ShellPtyDriver::new(options);
    (dir, workspace, transcript, driver)
}

/// P1 (c-ctxusage r16 item 6), barrier-after-closing-check: a promoted close
/// armed while a blocking transcript collect is still in flight must WAIT for
/// that poll to finish and for the poller to enter the non-terminal closing
/// branch before signalling the writer. Without the handoff barrier close ran
/// the stop ladder concurrently with the in-flight normal pump, which could
/// demote/finalise the hydrator under the writer shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promoted_close_waits_for_an_in_flight_poll_to_reach_the_closing_branch() {
    use remuda_driver::test_faults::TranscriptFaults;
    use std::io::Write;

    let (_dir, workspace, transcript, driver) = p1_promoted_layout();
    let driver = std::sync::Arc::new(driver);
    let faults = TranscriptFaults::new();
    driver.set_test_faults(faults.clone());

    // Bind first, without any gate.
    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");

    // Now arm the pump gate and append fresh tail bytes: the NEXT normal tick
    // enters its blocking collect and parks there, mid-flight in the NORMAL
    // (sample/demote/finalise) branch.
    faults.arm_pump_gate();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("open transcript for append");
    file.write_all(filler("race", 9001).as_bytes())
        .expect("append fresh record");
    file.sync_all().expect("sync append");
    drop(file);
    assert!(
        faults.wait_pump_entered(),
        "the post-bind normal-tick collect never parked"
    );

    // Close while that collect is parked. It must reach the barrier wait and
    // stay there — the stop ladder cannot run until the poll finishes and the
    // loop reaches the non-terminal closing branch.
    let mut close: tokio::task::JoinHandle<_> = tokio::spawn({
        let driver = driver.clone();
        async move { driver.close().await }
    });
    assert!(
        faults.wait_close_barrier_waiting_for(Duration::from_secs(15)),
        "close never parked at the poller handoff barrier"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut close)
            .await
            .is_err(),
        "close ran the stop ladder while the in-flight pump was still parked"
    );

    faults.release();
    let result = tokio::time::timeout(Duration::from_secs(30), close)
        .await
        .expect("close task panicked")
        .expect("close timed out after the pump released");
    result.expect("close completes once the poller reaches the closing branch");

    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed")
        .expect("event drainer panicked");
}

/// P1 (c-ctxusage r16 item 6), foreign drain: when the channel close arms is
/// not the one the running poller observes (the resumed-promotion defect —
/// start_resumed spawned a poller on a fresh LiveDrain instead of the
/// driver's adopted one), the handoff barrier can never be acknowledged. The
/// close must return the RETRYABLE WriterStillLive error WITHOUT signalling
/// the writer or retiring anything; restoring the drain the poller actually
/// holds lets a retried close complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_barrier_on_a_foreign_drain_is_retryable_and_a_retry_completes() {
    use remuda_driver::test_faults::TranscriptFaults;

    let (_dir, workspace, _transcript, driver) = p1_promoted_layout();
    let faults = TranscriptFaults::new();
    // Bound the wait so the missed-barrier case fails fast instead of 15 s.
    faults.arm_close_barrier_timeout(Duration::from_millis(1200));
    driver.set_test_faults(faults.clone());

    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");

    // Simulate the foreign-drain invariant violation: the poller keeps the
    // original drain while close arms a fresh one.
    let original = driver.test_swap_live_drain();
    let first = tokio::time::timeout(Duration::from_secs(15), driver.close())
        .await
        .expect("the first close hung past its bound");
    assert!(
        matches!(first, Err(remuda_driver::DriverError::WriterStillLive(_))),
        "a close whose barrier the poller never sees is retryable, got {first:?}"
    );

    // Restore the drain the live poller actually holds; the retry now reaches
    // the closing branch, acknowledges, and finishes the teardown.
    driver.test_restore_live_drain(original);
    tokio::time::timeout(Duration::from_secs(15), driver.close())
        .await
        .expect("the retried close hung past its bound")
        .expect("the retried close completes once the drain is the poller's");

    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed")
        .expect("event drainer panicked");
}

/// c-ctxusage r17 item 2: a promotion poller whose TASK HAS RETURNED (the
/// observation receiver was dropped, so events closed and the loop exited)
/// still occupies the driver's poller slot. Such a task has no in-flight
/// normal poll, so it satisfies the close handoff: close must signal the
/// writer immediately and complete once the process group is gone, instead
/// of waiting the barrier bound and returning WriterStillLive forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_returned_poller_does_not_block_a_promoted_close() {
    use remuda_driver::test_faults::TranscriptFaults;

    let (_dir, workspace, _transcript, driver) = p1_promoted_layout();
    let faults = TranscriptFaults::new();
    // Shrink the dead-barrier bound so the pre-fix code fails fast instead
    // of waiting the production 15 s before its retryable error.
    faults.arm_close_barrier_timeout(Duration::from_millis(800));
    driver.set_test_faults(faults.clone());

    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let events_rx = handle.into_events();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer = tokio::spawn(async move {
        let mut events_rx = events_rx;
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");

    // Close the observation channel (production: the run's event stream was
    // dropped while the native writer keeps running) and drive the poller
    // task to its RETURN; the driver slot stays occupied.
    drainer.abort();
    let _ = drainer.await;
    assert!(
        driver
            .test_wait_poller_exited(Duration::from_secs(15))
            .await,
        "the poller never exited after the observation channel closed"
    );
    assert_eq!(
        driver.test_poller_is_finished().await,
        Some(true),
        "the returned poller must still occupy its slot"
    );

    // The writer is alive and signable: close runs the ladder without a
    // barrier wait and completes once the process group is verified gone.
    tokio::time::timeout(Duration::from_secs(25), driver.close())
        .await
        .expect("close hung past its bound")
        .expect(
            "a finished poller satisfies the handoff; close signals the writer \
             and completes once the group is gone",
        );
}

/// c-ctxusage r17 item 3 (direct): a handoff oneshot that is CANCELED (its
/// sender is dropped without sending, as a concurrent close re-arming the
/// barrier would do) is not an acknowledgement. The close must stay
/// retryable and signal nothing; a later close with a fresh barrier
/// completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_canceled_close_barrier_is_retryable_not_acknowledged() {
    use remuda_driver::test_faults::TranscriptFaults;
    use std::io::Write;

    let (_dir, workspace, transcript, driver) = p1_promoted_layout();
    let driver = std::sync::Arc::new(driver);
    let faults = TranscriptFaults::new();
    driver.set_test_faults(faults.clone());

    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");

    // Park the next normal tick in its blocking collect, then start a close
    // that waits on the handoff barrier.
    faults.arm_pump_gate();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("open transcript for append");
    file.write_all(filler("cancel", 9002).as_bytes())
        .expect("append fresh record");
    file.sync_all().expect("sync append");
    drop(file);
    assert!(
        faults.wait_pump_entered(),
        "the post-bind normal-tick collect never parked"
    );
    let close = tokio::spawn({
        let driver = driver.clone();
        async move { driver.close().await }
    });
    assert!(
        faults.wait_close_barrier_waiting_for(Duration::from_secs(15)),
        "close never parked at the poller handoff barrier"
    );

    // Drop the barrier sender without an ack (the concurrent-close shape),
    // then release the parked poll. The Canceled must surface as a retryable
    // error, never as "acknowledged".
    assert!(
        driver.test_cancel_close_barrier().await,
        "no handoff sender was installed"
    );
    faults.release();
    let first = tokio::time::timeout(Duration::from_secs(15), close)
        .await
        .expect("the first close hung past its bound")
        .expect("the close task panicked");
    assert!(
        matches!(first, Err(remuda_driver::DriverError::WriterStillLive(_))),
        "a Canceled handoff is not an ack; close stays retryable, got {first:?}"
    );

    // A fresh close arms a new barrier the live poller observes and completes.
    tokio::time::timeout(Duration::from_secs(20), driver.close())
        .await
        .expect("the retried close hung past its bound")
        .expect("the retried close completes with a fresh handoff barrier");

    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed")
        .expect("event drainer panicked");
}

/// c-ctxusage r17 item 3 (serialization): two closes started while a normal
/// poll is parked must not arm two handoff barriers on the same drain — the
/// second arm would cancel the first close's oneshot and both stop ladders
/// would race the writer. The second close queues behind the first; after
/// the pump releases both complete and the writer's termination usage is
/// delivered exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_promoted_closes_share_one_handoff_and_stop_once() {
    use remuda_driver::test_faults::TranscriptFaults;
    use std::io::Write;

    let (_dir, workspace, transcript, driver) = p1_promoted_layout();
    let driver = std::sync::Arc::new(driver);
    let faults = TranscriptFaults::new();
    // Keep a timed-out handoff short if the teardown race parks the second
    // close on a barrier the retiring poller can no longer ack.
    faults.arm_close_barrier_timeout(Duration::from_millis(1200));
    driver.set_test_faults(faults.clone());

    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let term_count = Arc::new(AtomicUsize::new(0));
    let drainer_count = Arc::clone(&term_count);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id == TERMINATION_MSG_ID
            {
                drainer_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");

    faults.arm_pump_gate();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("open transcript for append");
    file.write_all(filler("concurrent", 9003).as_bytes())
        .expect("append fresh record");
    file.sync_all().expect("sync append");
    drop(file);
    assert!(
        faults.wait_pump_entered(),
        "the post-bind normal-tick collect never parked"
    );

    let first = tokio::spawn({
        let driver = driver.clone();
        async move { driver.close().await }
    });
    assert!(
        faults.wait_close_barrier_waiting_for(Duration::from_secs(15)),
        "the first close never parked at the handoff barrier"
    );
    let second = tokio::spawn({
        let driver = driver.clone();
        async move { driver.close().await }
    });

    // While the first handoff is unresolved the second close must be queued
    // behind it: no second barrier arm (the old code armed immediately,
    // canceling the first close's oneshot).
    assert!(
        !faults.wait_close_barrier_arms(2, Duration::from_millis(800)),
        "a concurrent close armed a second handoff barrier"
    );

    faults.release();
    for (name, task) in [("first", first), ("second", second)] {
        tokio::time::timeout(Duration::from_secs(40), task)
            .await
            .unwrap_or_else(|_| panic!("the {name} close timed out"))
            .unwrap_or_else(|e| panic!("the {name} close task panicked: {e:?}"))
            .unwrap_or_else(|e| panic!("the {name} close failed: {e:?}"));
    }

    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed")
        .expect("event drainer panicked");
    assert_eq!(
        term_count.load(Ordering::SeqCst),
        1,
        "the writer was stopped once: a single termination usage across both closes"
    );
}
