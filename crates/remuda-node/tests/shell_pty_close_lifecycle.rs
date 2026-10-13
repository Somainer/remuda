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
/// Message id of the record the test appends to the transcript AFTER a failed
/// promoted close, proving monitoring kept running (c-ctxusage r17 item 8).
const POST_FAILED_CLOSE_MSG_ID: &str = "msg_post_failed_close";
/// Message id of the record an in-flight normal poll is mapping when the close
/// barrier arms (c-ctxusage r17 item 8).
const IN_FLIGHT_RACE_MSG_ID: &str = "msg_in_flight_race";

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

/// A complete (end_turn) assistant record whose usage publishes on the very
/// next transcript poll — what a live writer flushing after a failed close or
/// an in-flight poll looks like (c-ctxusage r17 item 8). Distinct uuid and
/// message id so delivery is attributable to exactly this append.
fn flushing_record(message_id: &str, index: u32) -> String {
    let assistant = serde_json::json!({
        "type": "assistant",
        "uuid": format!("{message_id}-rec-{index}"),
        "timestamp": "2026-10-09T00:00:00.000Z",
        "isSidechain": false,
        "message": {
            "id": message_id,
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "flushed"}],
            "usage": {
                "input_tokens": 401 + (index as i64 % 50),
                "cache_read_input_tokens": 5_002,
                "cache_creation_input_tokens": 3,
                "output_tokens": 9
            }
        }
    });
    format!("{assistant}\n")
}

/// Wait for a usage observation with `scope_id == want` to be queued on the
/// drainer's scope channel, event-driven with a deadline as the sole bound (no
/// wall-clock poll). Scopes already collected are matched first.
async fn await_usage_scope(
    rx: &mut tokio::sync::mpsc::Receiver<String>,
    want: &str,
    mut seen: Vec<String>,
) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if seen.iter().any(|scope| scope == want) {
            return seen;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("usage scope {want} was never delivered; got {seen:?}");
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(scope)) => seen.push(scope),
            Ok(None) => panic!("the event channel closed before scope {want} arrived"),
            Err(_) => panic!("timed out waiting for usage scope {want}; got {seen:?}"),
        }
    }
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
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    // Every usage scope is forwarded to this channel: delivery is awaited
    // event-driven (bounded recv), never with a wall-clock poll.
    let (scope_tx, scope_rx) = tokio::sync::mpsc::channel::<String>(512);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body {
                let _ = scope_tx.send(payload.scope_id.clone()).await;
            }
        }
    });
    let mut scope_rx = scope_rx;

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

    // Monitoring kept running past the failed close: append a brand-new record
    // to the transcript AS AN EXTERNAL WRITER and prove the still-live poller
    // hydrates and delivers it. (c-ctxusage r17 item 8: the proof is a record
    // appended strictly AFTER the failed close returned, observed event-driven
    // — not a sleep-poll for the ladder's own SIGINT append.)
    use std::io::Write;
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .expect("reopen transcript for the post-failure append");
        file.write_all(flushing_record(POST_FAILED_CLOSE_MSG_ID, 1).as_bytes())
            .expect("append the post-failure record");
        file.sync_all().expect("sync the post-failure append");
    }
    let seen = await_usage_scope(&mut scope_rx, POST_FAILED_CLOSE_MSG_ID, Vec::new()).await;

    // The writer's own SIGINT termination append is delivered while the close
    // is incomplete as well (it may already be among the received scopes).
    let seen = await_usage_scope(&mut scope_rx, TERMINATION_MSG_ID, seen).await;

    // Attempt 2: the group is really gone, so the verification succeeds and
    // the teardown completes.
    tokio::time::timeout(Duration::from_secs(15), driver.close())
        .await
        .expect("the retried close hung past its bound")
        .expect("the retried close completes once the writer is verified gone");

    // Drain every scope the channel still holds until the drainer ends, then
    // assert delivery counts — both records exactly once across both close
    // attempts.
    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed after the retried close")
        .expect("event drainer panicked");
    let mut all = seen;
    while let Ok(scope) = tokio::time::timeout(Duration::from_secs(2), scope_rx.recv()).await {
        match scope {
            Some(scope) => all.push(scope),
            None => break,
        }
    }
    assert_eq!(
        all.iter()
            .filter(|scope| **scope == TERMINATION_MSG_ID)
            .count(),
        1,
        "the writer termination usage is delivered exactly once across both attempts"
    );
    assert_eq!(
        all.iter()
            .filter(|scope| **scope == POST_FAILED_CLOSE_MSG_ID)
            .count(),
        1,
        "the post-failure append is delivered exactly once"
    );
}

/// Build the promoted-claude fake-harness layout shared by the P1 race tests
/// (c-ctxusage r16 item 6).
fn p1_promoted_layout() -> (tempfile::TempDir, PathBuf, PathBuf, ShellPtyDriver) {
    p1_promoted_layout_inner(false)
}

/// Same layout, but each fake-harness process tags its termination record
/// with its own pid so a resumed run's append is distinguishable from the
/// predecessor's persisted record (c-ctxusage r17 item 4).
fn p1_promoted_layout_tagged() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, ShellPtyDriver) {
    let (dir, workspace, transcript, driver) = p1_promoted_layout_inner(true);
    let events_path = dir.path().join("harness-events.jsonl");
    (dir, workspace, transcript, events_path, driver)
}

fn p1_promoted_layout_inner(
    pid_tag: bool,
) -> (tempfile::TempDir, PathBuf, PathBuf, ShellPtyDriver) {
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
    if pid_tag {
        options
            .extra_env
            .insert("FAKE_HARNESS_TERMINATION_PID_TAG".into(), "1".into());
    }
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
///
/// c-ctxusage r17 item 8: the record that parked collect was mapping must be
/// ASSERTED DELIVERED (exactly once) — a finalise/demote interleaving the
/// writer shutdown used to discard it.
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
    let (scope_tx, scope_rx) = tokio::sync::mpsc::channel::<String>(64);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body {
                let _ = scope_tx.send(payload.scope_id.clone()).await;
            }
        }
    });
    let mut scope_rx = scope_rx;
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("the seeded replay never bound")
        .expect("bound oneshot closed without firing");

    // Now arm the pump gate and append fresh tail bytes: the NEXT normal tick
    // enters its blocking collect and parks there, mid-flight in the NORMAL
    // (sample/demote/finalise) branch, holding a record that must survive.
    faults.arm_pump_gate();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("open transcript for append");
    file.write_all(flushing_record(IN_FLIGHT_RACE_MSG_ID, 9001).as_bytes())
        .expect("append the in-flight record");
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

    // The parked poll's record was published, not eaten by a demote/finalise
    // interleaving the writer shutdown. Event-driven, bounded: the delivery
    // may have landed while the barrier was settling.
    let mut seen = Vec::new();
    while let Ok(scope) = tokio::time::timeout(Duration::from_millis(500), scope_rx.recv()).await
        && let Some(scope) = scope
    {
        seen.push(scope);
    }
    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed")
        .expect("event drainer panicked");
    while let Ok(scope) = tokio::time::timeout(Duration::from_secs(2), scope_rx.recv()).await
        && let Some(scope) = scope
    {
        seen.push(scope);
    }
    assert_eq!(
        seen.iter()
            .filter(|scope| **scope == IN_FLIGHT_RACE_MSG_ID)
            .count(),
        1,
        "the in-flight poll's record is delivered exactly once, not discarded; got {seen:?}"
    );
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

/// c-ctxusage r17 item 4: a RESUMED run must get a fresh LiveDrain. The
/// previous run's drain is permanently armed once close armed it
/// (`closing` is never reset), so cloning it for the resumed run put the
/// resumed poller in the non-terminal tail-only branch from its first tick:
/// no process sampling, no hook binding, no promotion — and the retained
/// driver's close armed a drain whose handoff could never be acknowledged.
/// This test actually RESUMES (the r16 variant only swapped drains): the
/// resumed run re-binds through its own drain, and a later close completes
/// through the fresh handoff with the termination append delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_run_uses_a_fresh_live_drain_and_rebinds() {
    use remuda_driver::test_faults::TranscriptFaults;

    let (_dir, workspace, _transcript, events_path, driver) = p1_promoted_layout_tagged();
    let driver = std::sync::Arc::new(driver);
    let faults = TranscriptFaults::new();
    // Keep a hypothetical stuck-barrier case fast; with the fix the resumed
    // close acks promptly and never reaches this bound.
    faults.arm_close_barrier_timeout(Duration::from_millis(1500));
    driver.set_test_faults(faults.clone());

    // ---- run 1: bind, then fully close (arms the run-1 drain forever) ----
    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let drainer1 = tokio::spawn(async move {
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
    tokio::time::timeout(Duration::from_secs(25), driver.close())
        .await
        .expect("run-1 close hung past its bound")
        .expect("run-1 close completes");
    tokio::time::timeout(Duration::from_secs(15), drainer1)
        .await
        .expect("run-1 event channel never closed")
        .expect("run-1 drainer panicked");
    assert!(
        driver.test_live_drain_is_closing(),
        "the closed run-1 drain stays armed (the state the resumed run must not inherit)"
    );

    // ---- run 2: real resume against the same native session id ----------
    let handle2 = driver
        .start_resumed(
            spec_for(&workspace, DriverKind::ShellPty),
            SESSION_ID.into(),
        )
        .await
        .expect("resume");
    assert!(
        !driver.test_live_drain_is_closing(),
        "the resumed run must start from a fresh, un-armed drain"
    );
    let mut events_rx2 = handle2.into_events();
    let (bound2_tx, bound2_rx) = tokio::sync::oneshot::channel();
    let bound2_tx = std::sync::Mutex::new(Some(bound2_tx));
    // The harness tags each process' termination record with the writer's
    // own pid, so the resumed writer's append is distinguishable from the
    // predecessor's persisted record that the resume replay re-hydrates.
    // Collect every tagged termination scope the run-2 channel carries.
    let term_scopes: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let term_scopes_drain = Arc::clone(&term_scopes);
    let drainer2 = tokio::spawn(async move {
        while let Some(observation) = events_rx2.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound2_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id.starts_with(TERMINATION_MSG_ID)
            {
                term_scopes_drain
                    .lock()
                    .unwrap()
                    .push(payload.scope_id.clone());
            }
        }
    });
    // With the stale drain the resumed poller is tail-only forever, so the
    // hook-driven bind never happens: this wait is the regression's bite.
    tokio::time::timeout(Duration::from_secs(25), bound2_rx)
        .await
        .expect("the resumed run never re-bound through its fresh drain")
        .expect("resume bound oneshot closed without firing");

    // ---- run 2 close goes through the FRESH handoff and drains once -----
    tokio::time::timeout(Duration::from_secs(25), driver.close())
        .await
        .expect("run-2 close hung past its bound")
        .expect("run-2 close completes via the resumed run's fresh handoff");
    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer2)
        .await
        .expect("run-2 event channel never closed")
        .expect("run-2 drainer panicked");

    // Both harness runs logged their pid in the shared events file; the LAST
    // session_start is the resumed writer. Its uniquely-tagged termination
    // usage must have been delivered exactly once (the predecessor's tag can
    // only arrive via the resume replay).
    let events_text = std::fs::read_to_string(&events_path).expect("harness events");
    let pid2 = events_text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row.get("event").and_then(serde_json::Value::as_str) == Some("session_start"))
        .filter_map(|row| row.get("pid").and_then(serde_json::Value::as_u64))
        .next_back()
        .expect("the resumed harness logged its pid");
    let scope2 = format!("{TERMINATION_MSG_ID}-{pid2}");
    let scopes = term_scopes.lock().unwrap();
    assert_eq!(
        scopes.iter().filter(|scope| **scope == scope2).count(),
        1,
        "the resumed writer's termination usage delivered exactly once, got {scopes:?}"
    );
}

/// c-ctxusage r18 item 3: a same-driver resume AFTER A NATURAL EXIT must
/// retire the predecessor run's promotion poller instead of overwriting and
/// detaching it. The writer's death only DEMOTES the poller (the shell is
/// still there); with no close() in between, resume used to drop the old
/// JoinHandle and its shutdown watch — the channel closed but the poll loop
/// read that closure as a no-op tick, so the predecessor poller ran forever,
/// tailing the old transcript and holding an event-sender clone (the old
/// run's receiver never closed).
///
/// Here run 1 is killed externally (no close), its natural `native_exit` is
/// observed, then a real resume starts run 2: the run-1 channel must close
/// (predecessor retired), run 2 must re-bind and close cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_natural_exit_before_resume_retires_the_predecessor_poller() {
    let (_dir, workspace, _transcript, events_path, driver) = p1_promoted_layout_tagged();
    let driver = std::sync::Arc::new(driver);

    // ---- run 1: bind, then let the writer die WITHOUT close() -----------
    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx1 = handle.into_events();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let exit_tx = std::sync::Mutex::new(Some(exit_tx));
    // Keep draining run 1 for the whole test: this task ends only when the
    // predecessor's last event sender drops during resume retirement.
    let drainer1 = tokio::spawn(async move {
        while let Some(observation) = events_rx1.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
            {
                if native.native_name == "transcript_bound"
                    && let Some(tx) = bound_tx.lock().unwrap().take()
                {
                    let _ = tx.send(());
                }
                if native.native_name == "native_exit"
                    && let Some(tx) = exit_tx.lock().unwrap().take()
                {
                    let _ = tx.send(());
                }
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("run 1 never bound")
        .expect("bound oneshot closed without firing");

    // External SIGTERM: a NATURAL death from the driver's perspective — no
    // close(), no stop ladder, the exit waiter reaps and reports native_exit.
    let events_text = std::fs::read_to_string(&events_path).expect("harness events");
    let pid1 = events_text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row.get("event").and_then(serde_json::Value::as_str) == Some("session_start"))
        .filter_map(|row| row.get("pid").and_then(serde_json::Value::as_u64))
        .next_back()
        .expect("run-1 harness logged its pid");
    let kill = std::process::Command::new("kill")
        .args(["-TERM", &pid1.to_string()])
        .status()
        .expect("spawn kill");
    assert!(kill.success(), "SIGTERM delivered to the run-1 writer");
    tokio::time::timeout(Duration::from_secs(20), exit_rx)
        .await
        .expect("the natural native_exit was never observed")
        .expect("exit oneshot closed without firing");

    // ---- run 2: real resume, still no intervening close ----------------
    let handle2 = driver
        .start_resumed(
            spec_for(&workspace, DriverKind::ShellPty),
            SESSION_ID.into(),
        )
        .await
        .expect("resume after a natural exit");
    let mut events_rx2 = handle2.into_events();
    let (bound2_tx, bound2_rx) = tokio::sync::oneshot::channel();
    let bound2_tx = std::sync::Mutex::new(Some(bound2_tx));
    let drainer2 = tokio::spawn(async move {
        while let Some(observation) = events_rx2.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound2_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
        }
    });

    // The predecessor poller (and every old run sender) was retired during
    // resume: run 1's channel CLOSES, bounded, instead of staying held open by
    // the detached poller forever.
    tokio::time::timeout(Duration::from_secs(20), drainer1)
        .await
        .expect("run-1 channel never closed: the predecessor poller leaked past resume")
        .expect("run-1 drainer panicked");

    // The new run binds through its own fresh drain/poller.
    tokio::time::timeout(Duration::from_secs(25), bound2_rx)
        .await
        .expect("run 2 never re-bound after resume")
        .expect("resume bound oneshot closed without firing");

    // Run 2 closes through its own handoff and fully retires.
    tokio::time::timeout(Duration::from_secs(25), driver.close())
        .await
        .expect("run-2 close hung past its bound")
        .expect("run-2 close completes");
    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer2)
        .await
        .expect("run-2 channel never closed")
        .expect("run-2 drainer panicked");
}

/// c-ctxusage r19/r20/r21: same-driver resume after a natural exit must spawn
/// the new run on its OWN detection AND binding state BEFORE retiring the
/// predecessor, and a post-death normal sample must never race the
/// predecessor's finalise.
///
/// Deterministic, acknowledged barriers (c-ctxusage r21 item 3): a test-only
/// gate holds NORMAL samples — while letting shutdown/live-drain/close wakeups
/// through — engaged before run 1 is killed, so no post-death demote sample can
/// beat the predecessor finalise park.
///
///  1. run 1 binds; BOTH the normal-sample gate and the finish gate are armed;
///     run 1 is killed; its post-death poll parks at the normal gate.
///  2. resume spawns run 2; the driver ACK-waits for run 2's poller (two
///     poller starts) before retiring; run 2's first normal sample parks at
///     the same gate; retirement parks run 1 in its terminal finalise.
///  3. HARD assertions at that park: exactly two pollers started, one more
///     normal sample parked since resume, the current promoted slot is still
///     run 1's, and run 2's fresh BindingHandle is NOT bound to run 1's
///     transcript (c-ctxusage r21 item 1).
///  4. release the finish gate: predecessor retires and run 2's handles are
///     adopted. Release the normal gate: run 2 binds through its OWN handle;
///     the current slot changes and is promoted.
///  5. re-arm the normal gate and close immediately: only shutdown/drain
///     wakeups reach the poller; the PROMOTED draining close delivers run 2's
///     tagged termination usage exactly once.
///
/// Reverting the spawn/retire ordering leaves one poller at the park (hard
/// count failure); reverting per-run bindings leaves run 2 bound to the
/// predecessor's transcript at step 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retirement_after_run2_binds_does_not_clear_run2_promotion() {
    use remuda_driver::test_faults::TranscriptFaults;
    let (_dir, workspace, _transcript, events_path, driver) = p1_promoted_layout_tagged();
    let driver = std::sync::Arc::new(driver);
    let faults = TranscriptFaults::new();
    // Finish gate parks the predecessor terminal finalise during retirement;
    // the normal-sample gate freezes every normal poll (close/drain/shutdown
    // still pass). A short retire bound keeps any failure fast.
    faults.arm_finish_gate();
    faults.arm_close_timeout(Duration::from_secs(5));
    driver.set_test_faults(faults.clone());

    // ---- run 1 ----------------------------------------------------------
    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx1 = handle.into_events();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let exit_tx = std::sync::Mutex::new(Some(exit_tx));
    let drainer1 = tokio::spawn(async move {
        while let Some(observation) = events_rx1.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
            {
                if native.native_name == "transcript_bound"
                    && let Some(tx) = bound_tx.lock().unwrap().take()
                {
                    let _ = tx.send(());
                }
                if native.native_name == "native_exit"
                    && let Some(tx) = exit_tx.lock().unwrap().take()
                {
                    let _ = tx.send(());
                }
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("run 1 never bound")
        .expect("bound oneshot closed without firing");
    let predecessor_slot = driver.test_promoted_slot_id();
    assert!(
        driver.test_bindings_is_bound(),
        "run 1 established a binding claim"
    );

    // Freeze normal samples BEFORE the death: the post-death demote tick (and
    // run 2's first tick) park here instead of racing the finalise.
    faults.arm_normal_sample_gate();
    let pid1 = last_session_pid(&events_path);
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid1.to_string()])
            .status()
            .expect("spawn kill")
            .success(),
        "SIGTERM delivered to the run-1 writer"
    );
    tokio::time::timeout(Duration::from_secs(20), exit_rx)
        .await
        .expect("the natural native_exit was never observed")
        .expect("exit oneshot closed without firing");
    // The predecessor's post-death normal sample is held.
    assert!(
        faults.wait_normal_samples_parked(1, Duration::from_secs(20)),
        "run 1's post-death normal sample never parked"
    );
    let parked_before_resume = faults.normal_sample_park_count();

    // ---- resume ---------------------------------------------------------
    let mut resume = tokio::spawn({
        let driver = driver.clone();
        async move {
            driver
                .start_resumed(
                    spec_for(&workspace, DriverKind::ShellPty),
                    SESSION_ID.into(),
                )
                .await
        }
    });
    assert!(
        faults.wait_finish_entered(),
        "the predecessor terminal finalise never parked during retirement"
    );

    // HARD park assertions.
    assert_eq!(
        faults.promoter_start_count(),
        2,
        "at the park both poller tasks exist (spawn-before-retire); reverting          the order leaves exactly one here"
    );
    assert!(
        faults.wait_normal_samples_parked(parked_before_resume + 1, Duration::from_secs(20)),
        "run 2's first normal sample never parked while the gate was held"
    );
    assert_eq!(
        driver.test_promoted_slot_id(),
        predecessor_slot,
        "the current promoted slot is still run 1's at the park"
    );
    // The current BINDING handle is still run 1's here (run 2's fresh handle
    // is adopted only after retirement); the per-run-binding assertion is made
    // below once adoption has happened but before run 2 has sampled.

    // Release the predecessor finalise: retirement completes and run 2's
    // handles are adopted; resume returns.
    faults.release();
    let handle2 = tokio::time::timeout(Duration::from_secs(30), &mut resume)
        .await
        .expect("resume hung past its bound")
        .expect("resume task panicked")
        .expect("resume completes after predecessor retirement");
    assert!(
        !driver.test_bindings_is_bound(),
        "run 2's adopted handle is still unbound before its own first sample"
    );

    // Let run 2 run its own normal sample(s) and bind via its OWN handle.
    faults.release_normal_samples();
    assert!(
        faults.wait_promoter_binds(2, Duration::from_secs(20)),
        "run 2 never bound through its own BindingHandle"
    );
    assert_ne!(
        driver.test_promoted_slot_id(),
        predecessor_slot,
        "the current slot is run 2's after adoption"
    );
    assert_eq!(
        driver.promoted_kind(),
        Some(AgentKind::Claude),
        "run 2's promotion is current"
    );
    assert!(
        driver.test_bindings_is_bound(),
        "run 2's handle is now bound"
    );

    // Re-freeze normal samples and close immediately: the promoted close must
    // run purely on shutdown/drain wakeups and still drain the writer.
    faults.arm_normal_sample_gate();

    // ---- run 2 events + close ------------------------------------------
    let mut events_rx2 = handle2.into_events();
    let (bound2_tx, bound2_rx) = tokio::sync::oneshot::channel();
    let bound2_tx = std::sync::Mutex::new(Some(bound2_tx));
    let (scope_tx, scope_rx) = tokio::sync::mpsc::channel::<String>(64);
    let drainer2 = tokio::spawn(async move {
        while let Some(observation) = events_rx2.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound2_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body
                && payload.scope_id.starts_with(TERMINATION_MSG_ID)
            {
                let _ = scope_tx.send(payload.scope_id.clone()).await;
            }
        }
    });
    let mut scope_rx = scope_rx;
    tokio::time::timeout(Duration::from_secs(20), bound2_rx)
        .await
        .expect("run 2 never bound")
        .expect("run-2 bound oneshot closed without firing");

    tokio::time::timeout(Duration::from_secs(25), driver.close())
        .await
        .expect("close hung past its bound")
        .expect("close takes the promoted draining path and completes");

    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer1)
        .await
        .expect("run-1 channel never closed")
        .expect("run-1 drainer panicked");
    tokio::time::timeout(Duration::from_secs(30), drainer2)
        .await
        .expect("run-2 channel never closed")
        .expect("run-2 drainer panicked");

    let pid2 = last_session_pid(&events_path);
    assert_ne!(pid1, pid2, "resume started a distinct harness process");
    let scope2 = format!("{TERMINATION_MSG_ID}-{pid2}");
    let mut scopes = Vec::new();
    while let Ok(scope) = tokio::time::timeout(Duration::from_millis(500), scope_rx.recv()).await
        && let Some(scope) = scope
    {
        scopes.push(scope);
    }
    assert_eq!(
        scopes.iter().filter(|scope| **scope == scope2).count(),
        1,
        "run 2 termination usage delivered exactly once; got {scopes:?}"
    );
}

/// Last `session_start` pid in the shared harness events file.
fn last_session_pid(events_path: &Path) -> u64 {
    let events_text = std::fs::read_to_string(events_path).expect("harness events");
    events_text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row.get("event").and_then(serde_json::Value::as_str) == Some("session_start"))
        .filter_map(|row| row.get("pid").and_then(serde_json::Value::as_u64))
        .next_back()
        .expect("a harness session_start pid")
}

/// c-ctxusage r20 item 1: a same-driver resume whose NEW run fails to spawn
/// must leave the predecessor run completely intact — its poller still tails
/// the transcript, its detection slots still hold the live writer, and a
/// later close() takes the promoted path. Retirement must happen only AFTER a
/// successful spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_resume_spawn_leaves_the_predecessor_run_intact() {
    use remuda_driver::test_faults::TranscriptFaults;
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
    let (scope_tx, scope_rx) = tokio::sync::mpsc::channel::<String>(64);
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
                && native.native_name == "transcript_bound"
                && let Some(tx) = bound_tx.lock().unwrap().take()
            {
                let _ = tx.send(());
            }
            if let ObservationPayload::Usage(payload) = &observation.body {
                let _ = scope_tx.send(payload.scope_id.clone()).await;
            }
        }
    });
    let mut scope_rx = scope_rx;
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("run 1 never bound")
        .expect("bound oneshot closed without firing");
    assert_eq!(
        driver.promoted_kind(),
        Some(AgentKind::Claude),
        "run 1 is promoted before the failed resume"
    );

    // Arm ONE post-materialize spawn failure, then resume. The new run never
    // starts a child and the resume must return the error WITHOUT retiring
    // run 1.
    faults.arm_spawn_failures(1);
    let failed = tokio::time::timeout(Duration::from_secs(30), async {
        driver
            .start_resumed(
                spec_for(&workspace, DriverKind::ShellPty),
                SESSION_ID.into(),
            )
            .await
    })
    .await
    .expect("the failed resume hung past its bound");
    assert!(
        failed.is_err(),
        "the injected spawn failure must surface: {failed:?}"
    );

    // The predecessor's detection slots are untouched.
    assert_eq!(
        driver.promoted_kind(),
        Some(AgentKind::Claude),
        "the failed resume must not clear the predecessor's promotion slot"
    );

    // The predecessor poller still tails: an external append after the failed
    // resume is delivered event-driven (bounded recv, no sleep).
    use std::io::Write;
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .expect("reopen transcript");
        file.write_all(flushing_record(POST_FAILED_CLOSE_MSG_ID, 7).as_bytes())
            .expect("append after the failed resume");
        file.sync_all().expect("sync");
    }
    let mut seen = Vec::new();
    seen = await_usage_scope(&mut scope_rx, POST_FAILED_CLOSE_MSG_ID, seen).await;

    // close() still takes the PROMOTED path: the live writer is signalled, its
    // termination append drained, and teardown completes.
    tokio::time::timeout(Duration::from_secs(25), driver.close())
        .await
        .expect("close hung past its bound")
        .expect("close takes the promoted path after a failed resume");

    driver.abort_event_workers_for_test().await;
    tokio::time::timeout(Duration::from_secs(30), drainer)
        .await
        .expect("event channel never closed")
        .expect("drainer panicked");
    while let Ok(scope) = tokio::time::timeout(Duration::from_secs(2), scope_rx.recv()).await
        && let Some(scope) = scope
    {
        seen.push(scope);
    }
    assert_eq!(
        seen.iter()
            .filter(|scope| **scope == TERMINATION_MSG_ID)
            .count(),
        1,
        "the promoted close delivered run 1's termination usage exactly once"
    );
    assert_eq!(
        seen.iter()
            .filter(|scope| **scope == POST_FAILED_CLOSE_MSG_ID)
            .count(),
        1,
        "the post-failed-resume append was delivered by the live predecessor poller"
    );
}

/// c-ctxusage r21 item 4: when predecessor retirement reaches its bounded
/// wait while the predecessor finalise is parked in the injected finish gate,
/// retirement must RELEASE that gate so the finalise (and the poller)
/// completes inside the bound — not abandon a parked blocking thread behind an
/// abort that cannot cancel a spawn_blocking closure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retire_bound_releases_a_parked_finalise_and_completes() {
    use remuda_driver::test_faults::TranscriptFaults;
    let (_dir, workspace, _transcript, events_path, driver) = p1_promoted_layout_tagged();
    let driver = std::sync::Arc::new(driver);
    let faults = TranscriptFaults::new();
    faults.arm_finish_gate();
    // 500 ms retire bound: the fix releases the parked gate at the bound, so
    // retirement finishes shortly after; the revert path aborts and then waits
    // the full 2 s post-abort join timeout (it cannot cancel a parked
    // spawn_blocking), ~2.5 s total.
    faults.arm_close_timeout(Duration::from_millis(500));
    driver.set_test_faults(faults.clone());

    let handle = driver
        .start(spec_for(&workspace, DriverKind::ShellPty))
        .await
        .expect("start");
    let mut events_rx = handle.into_events();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let bound_tx = std::sync::Mutex::new(Some(bound_tx));
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let exit_tx = std::sync::Mutex::new(Some(exit_tx));
    let drainer = tokio::spawn(async move {
        while let Some(observation) = events_rx.recv().await {
            if let ObservationPayload::Lifecycle(lc) = &observation.body
                && let remuda_protocol::LifecyclePayload::Native(native) = lc.as_ref()
            {
                if native.native_name == "transcript_bound"
                    && let Some(tx) = bound_tx.lock().unwrap().take()
                {
                    let _ = tx.send(());
                }
                if native.native_name == "native_exit"
                    && let Some(tx) = exit_tx.lock().unwrap().take()
                {
                    let _ = tx.send(());
                }
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(20), bound_rx)
        .await
        .expect("run 1 never bound")
        .expect("bound");
    let pid1 = last_session_pid(&events_path);
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid1.to_string()])
            .status()
            .expect("spawn kill")
            .success()
    );
    tokio::time::timeout(Duration::from_secs(20), exit_rx)
        .await
        .expect("native_exit never observed")
        .expect("exit");

    // Resume WITHOUT releasing the finish gate: retirement hits its bound and
    // must release the parked finalise itself.
    let started = Instant::now();
    let resume = tokio::time::timeout(
        Duration::from_secs(10),
        driver.start_resumed(
            spec_for(&workspace, DriverKind::ShellPty),
            SESSION_ID.into(),
        ),
    )
    .await
    .expect("resume hung")
    .expect("resume ok");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(2000),
        "retirement completed within its bound: {elapsed:?}"
    );
    // The hard item-4 signal: the test never called release(), so the ONLY
    // thing that opened the blocking gate is retirement releasing the parked
    // finalise at its bound. The revert abandons the parked thread and leaves
    // the gate shut.
    assert!(
        faults.blocking_gate_is_open(),
        "retirement at its bound must RELEASE the parked finalise (gate open), \
         not abandon the parked blocking thread (gate still shut)"
    );

    // Clean up the resumed run; the released predecessor is already retired.
    faults.release();
    drop(resume);
    tokio::time::timeout(Duration::from_secs(25), driver.close())
        .await
        .expect("close hung")
        .expect("close ok");
    driver.abort_event_workers_for_test().await;
    let _ = tokio::time::timeout(Duration::from_secs(30), drainer).await;
}
