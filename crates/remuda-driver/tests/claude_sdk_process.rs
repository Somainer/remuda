//! Process tests for `ClaudeSdkDriver` against `fake-claude`.
//!
//! The claim under test is the one `claude-print` cannot make: the child
//! survives more than one turn, because the sdk argv has no `-p`
//! (`print-replacement.md` §2.1, §3 batches 1-2). Same fake, same scripts, same
//! mapper — so a difference here is a difference in the carrier, not the double.

use remuda_driver::claude_sdk::{ClaudeSdkDriver, ClaudeSdkOptions};
use remuda_driver::{
    BinaryPin, BinarySource, Driver, DriverError, ProviderHealth, ProviderKind, ProviderProfile,
    SecretRef,
};
use remuda_protocol::{
    ContentBlock, Digest, DispatchState, DriverInput, DriverKind, InputOrigin, InstanceSpec,
    Knowledge, LifecyclePayload, Observation, ObservationPayload, PromptInput, PromptMode,
    SourceChannel, TextBlock,
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

fn dummy_digest() -> Digest {
    Digest::try_from(format!("sha256:{:0>64}", "a")).unwrap()
}

fn pin_fake() -> BinaryPin {
    let path = ensure_fake_claude().canonicalize().unwrap();
    BinaryPin {
        abs_path: path.to_string_lossy().into_owned(),
        version: "fake-claude".into(),
        sha256: dummy_digest(),
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

/// The shared spec with `driver` switched to the carrier under test.
fn load_spec() -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = DriverKind::ClaudeSdk;
    spec
}

fn driver_for(kind: ScriptKind) -> (tempfile::TempDir, ClaudeSdkDriver, InstanceSpec) {
    driver_with_env(kind, BTreeMap::new())
}

/// [`driver_for`] plus extra child env (`FAKE_CLAUDE_*` knobs).
fn driver_with_env(
    kind: ScriptKind,
    env: BTreeMap<String, String>,
) -> (tempfile::TempDir, ClaudeSdkDriver, InstanceSpec) {
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
    extra.extend(env);
    let mut options =
        ClaudeSdkOptions::new(profile(), launch, home, BinarySource::Pinned(pin_fake()));
    options.origin = InputOrigin::Human;
    options.extra_env = extra;
    options.handshake_timeout = Duration::from_secs(5);
    options.close_timeout = Duration::from_secs(3);
    let mut spec = load_spec();
    spec.cwd = tmp
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    (tmp, ClaudeSdkDriver::new(options), spec)
}

/// Whether the child named by the launch ack is still running.
///
/// `spawn_command` puts the child in its own process group, so its pid is also
/// its pgid and the existing `kill(-pgid, 0)` probe answers this without parsing
/// `ps`. A process we may not signal still counts as alive, which is the honest
/// answer for this assertion.
fn process_alive(pid: &str) -> bool {
    let Ok(pid) = pid.parse::<i32>() else {
        return false;
    };
    remuda_driver::shell_pty::lifecycle::group_alive(pid)
}

fn prompt(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "msg-1".into(),
    }))
}

async fn collect_until(
    handle: &mut remuda_driver::RunHandle,
    timeout: Duration,
    mut pred: impl FnMut(&[Observation]) -> bool,
) -> Vec<Observation> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut out = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, handle.recv()).await {
            Ok(Some(obs)) => {
                out.push(obs);
                if pred(&out) {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    out
}

fn lifecycle_status(obs: &Observation) -> Option<&str> {
    match &obs.body {
        ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
            LifecyclePayload::Native(native) => match &native.status {
                Knowledge::Known { value } => Some(value.as_str()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn lifecycle_named(obs: &Observation) -> Option<&str> {
    match &obs.body {
        ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
            LifecyclePayload::Native(native) => Some(native.native_name.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn turn_done_count(obs: &[Observation]) -> usize {
    obs.iter()
        .filter(|o| lifecycle_status(o) == Some("turn_done"))
        .count()
}

/// Golden argv, read back from the child itself.
///
/// The earlier version of this test asserted against `fake_claude_argv`, a
/// literal the driver never reads — so it could only ever prove the literal was
/// self-consistent. The fake now records its own argv, so this asserts on what
/// the process was actually launched with, which is the thing that regresses if
/// `-p` ever comes back (§2.1).
#[tokio::test]
async fn the_child_is_launched_without_dash_p() {
    let recorded = std::env::temp_dir().join(format!(
        "remuda-sdk-argv-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_CLAUDE_ARGV_FILE".into(),
        recorded.to_string_lossy().into_owned(),
    );
    let (_tmp, driver, spec) = driver_with_env(ScriptKind::Ok, env);
    let _handle = driver.start(spec).await.expect("start");

    let argv: Vec<String> = std::fs::read_to_string(&recorded)
        .expect("fake recorded its argv")
        .lines()
        .map(str::to_owned)
        .collect();
    let _ = std::fs::remove_file(&recorded);

    assert!(
        !argv.iter().any(|a| a == "-p" || a == "--print"),
        "the child must not print-and-exit: {argv:?}"
    );
    for flag in [
        "--input-format",
        "--output-format",
        "stream-json",
        "--permission-prompt-tool",
        "stdio",
    ] {
        assert!(argv.iter().any(|a| a == flag), "missing {flag} in {argv:?}");
    }
    // §2.1: never these, on any template.
    for flag in [
        "--bare",
        "--safe-mode",
        "--no-session-persistence",
        "--continue",
    ] {
        assert!(!argv.iter().any(|a| a == flag), "child argv has {flag}");
    }
    driver.close().await.expect("close");
}

/// One turn end to end: the §2.5 observation table, stamped `claude-sdk` on
/// `stdout`, so the Structural view renders with no web work.
#[tokio::test]
async fn one_turn_emits_the_observation_sequence_in_order() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::Ok);
    let mut handle = driver.start(spec).await.expect("start");
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);
    driver.send(prompt("hi")).await.expect("send");
    // `map_result` emits the turn lifecycle and then its usage, so waiting on
    // `turn_done` alone would stop one observation short.
    let events = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 1
            && obs
                .iter()
                .any(|o| matches!(o.body, ObservationPayload::Usage(_)))
    })
    .await;

    // Every observation carries this carrier's identity (§2.5).
    for obs in &events {
        assert_eq!(obs.source.driver_kind, DriverKind::ClaudeSdk);
        assert_eq!(obs.source.channel, SourceChannel::Stdout);
    }

    // Ordering: the session lifecycle from `system/init` precedes the assembled
    // message, which precedes the turn's `result` and its usage.
    let session_at = events
        .iter()
        .position(|o| {
            lifecycle_named(o) == Some("session") && lifecycle_status(o) == Some("started")
        })
        .expect("session started lifecycle");
    let message_at = events
        .iter()
        .position(|o| matches!(o.body, ObservationPayload::Message(_)))
        .expect("assistant message");
    let result_at = events
        .iter()
        .position(|o| lifecycle_status(o) == Some("turn_done"))
        .expect("turn_done");
    assert!(
        session_at < message_at && message_at < result_at,
        "out of order: session={session_at} message={message_at} result={result_at}"
    );
    // usage rides the terminal result (§2.5, `usage_from_result`).
    let usage_at = events
        .iter()
        .position(|o| matches!(o.body, ObservationPayload::Usage(_)))
        .expect("usage from result");
    assert!(usage_at > message_at, "usage must follow the message");

    driver.close().await.expect("close");
}

/// The carrier's reason to exist: a second `send` lands on the **same** child.
///
/// A `-p` process is gone after the first `result`, so print cannot do this
/// (§2.1, D-035). In-session turns never resume — they are further `user` frames
/// on the open stdin (§2.2) — so the session id and the pid must both hold.
#[tokio::test]
async fn a_second_send_reaches_the_same_process_and_session() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::TwoTurn);
    let mut handle = driver.start(spec).await.expect("start");
    let pid = handle
        .ack()
        .native_ids
        .get("pid")
        .expect("pid on the launch ack")
        .clone();

    driver.send(prompt("first")).await.expect("first send");
    let first = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 1
    })
    .await;
    assert_eq!(turn_done_count(&first), 1, "expected one turn to finish");
    let session = first
        .iter()
        .find_map(|o| match &o.source.native_session_id {
            Knowledge::Known { value } if !value.is_empty() => Some(value.clone()),
            _ => None,
        })
        .expect("native session id");

    // The child must still be running *between* the turns — that is the property
    // print does not have, and checking it here (rather than only at the end)
    // pins where a regression would appear.
    assert!(
        process_alive(&pid),
        "pid {pid} exited after the first turn: the child did not survive it"
    );

    // No relaunch, no resume: the same driver object, the same live stdin.
    driver.send(prompt("second")).await.expect("second send");
    let second = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 1
    })
    .await;
    assert_eq!(
        turn_done_count(&second),
        1,
        "second send produced no result: the child did not survive the first turn"
    );
    for obs in &second {
        assert_eq!(obs.source.driver_kind, DriverKind::ClaudeSdk);
        if let Knowledge::Known { value } = &obs.source.native_session_id {
            assert_eq!(value, &session, "session id changed across turns");
        }
    }

    // Same OS process, observed across both turns. The launch ack is a snapshot
    // taken once, so comparing it with itself proves nothing; what matters is
    // that the process it named is still the live child after the second turn.
    assert!(
        process_alive(&pid),
        "pid {pid} is gone after two turns: the child did not survive, which is \
         exactly the print defect this carrier exists to fix"
    );
    let live_pin = driver.capabilities().await.expect("capabilities");
    assert_eq!(live_pin.driver_kind, DriverKind::ClaudeSdk);

    driver.close().await.expect("close");
    // And the ladder actually reaped it.
    assert!(
        !process_alive(&pid),
        "pid {pid} outlived close: the ladder did not reap the child"
    );
}

/// Resume is a **new** process with `--resume <id>` (D-026); the exited child is
/// never resurrected and `--continue` is never passed.
#[tokio::test]
async fn start_resumed_passes_resume_on_a_new_child() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::Ok);
    let session = "77777777-7777-4777-8777-777777777777";
    let handle = driver
        .start_resumed(spec, session.to_string())
        .await
        .expect("start_resumed");
    let argv = &handle.recipe().argv;
    let at = argv
        .iter()
        .position(|a| a == "--resume")
        .unwrap_or_else(|| panic!("no --resume in {argv:?}"));
    assert_eq!(argv.get(at + 1).map(String::as_str), Some(session));
    assert!(
        !argv.iter().any(|a| a == "--session-id"),
        "resume must not also mint a new session: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a == "-p" || a == "--continue"),
        "{argv:?}"
    );
    driver.close().await.expect("close");
}

/// A missing or empty session id is an error, never a silent empty
/// conversation (§2.2 item 4).
#[tokio::test]
async fn start_resumed_without_a_session_id_is_refused() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::Ok);
    let error = driver
        .start_resumed(spec, "   ".to_string())
        .await
        .expect_err("empty session id");
    assert!(
        matches!(error, DriverError::NativeSessionNotFound),
        "{error:?}"
    );
}

/// There is no PTY on this carrier, so the TTY surface stays unsupported rather
/// than showing an empty grid (§2.6).
#[tokio::test]
async fn tty_surface_is_unsupported() {
    let (_tmp, driver, _spec) = driver_for(ScriptKind::Ok);
    assert!(matches!(
        driver.write_tty(b"x").await,
        Err(DriverError::CapabilityUnsupported(_))
    ));
    assert!(matches!(
        driver.send_keys(vec!["enter".into()]).await,
        Err(DriverError::CapabilityUnsupported(_))
    ));
    assert!(driver.screen_read().await.expect("screen_read").is_none());
    assert!(driver.tty_bridge().await.is_none());
}

/// §2.3: `close` is a bounded ladder, not an unbounded `wait`.
///
/// The child here ignores stdin EOF, which is exactly what a real one does when
/// it is mid-turn or blocked on an unanswered `can_use_tool`. The previous
/// implementation called `child.wait()` with no bound while holding `inner.live`,
/// so `instance.close` on the long-lived sdk carrier would hang forever and take
/// every other control operation with it. Close must return inside the bound
/// with the child gone.
#[tokio::test]
async fn close_returns_within_the_bound_when_the_child_ignores_stdin_eof() {
    let mut env = BTreeMap::new();
    env.insert("FAKE_CLAUDE_IGNORE_EOF".into(), "1".into());
    let (_tmp, driver, spec) = driver_with_env(ScriptKind::Ok, env);
    let mut handle = driver.start(spec).await.expect("start");
    let pid = handle
        .ack()
        .native_ids
        .get("pid")
        .expect("pid on the launch ack")
        .clone();
    assert!(process_alive(&pid), "child should be running before close");

    // `close_timeout` is 3s in this harness; allow the three rungs plus the
    // final reap, and still fail well before a hang would look like a pass.
    let started = std::time::Instant::now();
    let closed = tokio::time::timeout(Duration::from_secs(20), driver.close()).await;
    let elapsed = started.elapsed();

    assert!(
        closed.is_ok(),
        "close did not return: the ladder is unbounded again"
    );
    closed.unwrap().expect("close ack");
    assert!(
        elapsed < Duration::from_secs(15),
        "close took {elapsed:?}, which is not a bounded ladder"
    );
    assert!(
        !process_alive(&pid),
        "pid {pid} survived close: the ladder never reached SIGKILL"
    );

    // Exactly one `exited` lifecycle, and it is still delivered even though the
    // child had to be killed — a consumer must not be left without it.
    let mut exits = 0;
    while let Ok(Some(obs)) = tokio::time::timeout(Duration::from_millis(200), handle.recv()).await
    {
        if lifecycle_named(&obs) == Some("session") && lifecycle_status(&obs) == Some("exited") {
            exits += 1;
        }
    }
    assert_eq!(exits, 1, "expected exactly one session `exited` lifecycle");
}

/// A child that leaves on its own must not be killed, and must still report one
/// `exited` lifecycle — the reader task and `close` race for it.
#[tokio::test]
async fn a_cooperative_child_exits_on_stdin_eof_with_one_exit_lifecycle() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::Ok);
    let mut handle = driver.start(spec).await.expect("start");
    let pid = handle.ack().native_ids.get("pid").expect("pid").clone();
    driver.send(prompt("hi")).await.expect("send");
    let _ = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 1
    })
    .await;

    let started = std::time::Instant::now();
    driver.close().await.expect("close");
    // The first slice is 50% of the 3s budget (1.5s); a cooperative child that
    // leaves on stdin EOF returns well inside it, before SIGTERM is sent.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a cooperative child should not have waited for the SIGTERM rung"
    );
    assert!(!process_alive(&pid), "child should be reaped");

    let mut exits = 0;
    while let Ok(Some(obs)) = tokio::time::timeout(Duration::from_millis(200), handle.recv()).await
    {
        if lifecycle_named(&obs) == Some("session") && lifecycle_status(&obs) == Some("exited") {
            exits += 1;
        }
    }
    assert_eq!(exits, 1, "expected exactly one session `exited` lifecycle");
}

/// The deepest rung: a child that ignores both stdin EOF *and* SIGTERM must
/// still be gone when `close` returns (§2.3).
///
/// The cooperative and EOF-ignoring cases above stop at rungs 1 and 2, so
/// without this the SIGKILL rung would be untested — and that is the rung that
/// exists for a wedged child holding the workspace open.
///
/// Two independent guards make this non-vacuous:
///
/// * **SIGTERM ordering** — SIGTERM is masked before any thread spawns, and the
///   test asserts the child outlived *both* bounded slices (≥ 3s). Masking it on
///   the wrong thread lets the process-directed signal kill the fake at rung 2,
///   and close then returns after one slice.
/// * **The group SIGKILL itself** — the fake spawns a long-lived grandchild in
///   its process group. `Child::kill` and `kill_on_drop` reach only the direct
///   child, so the grandchild survives deleting rung 3's `killpg`; the elapsed
///   assertion alone could not tell the group signal apart from `kill_on_drop`
///   reaping the direct child. The grandchild dying is the structural guard.
#[cfg(unix)]
#[tokio::test]
async fn close_kills_a_child_that_ignores_both_eof_and_sigterm() {
    let grandchild_pid_file = std::env::temp_dir().join(format!(
        "remuda-sdk-grandchild-{}-{}.pid",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut env = BTreeMap::new();
    env.insert("FAKE_CLAUDE_IGNORE_EOF".into(), "1".into());
    env.insert("FAKE_CLAUDE_IGNORE_SIGTERM".into(), "1".into());
    env.insert(
        "FAKE_CLAUDE_GRANDCHILD_PID_FILE".into(),
        grandchild_pid_file.to_string_lossy().into_owned(),
    );
    let (_tmp, driver, spec) = driver_with_env(ScriptKind::Ok, env);
    let handle = driver.start(spec).await.expect("start");
    let pid = handle
        .ack()
        .native_ids
        .get("pid")
        .expect("pid on the launch ack")
        .clone();
    assert!(process_alive(&pid), "child should be running before close");

    // Read the grandchild the fake spawned in its own group.
    let grandchild = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&grandchild_pid_file)
                && let Ok(gpid) = text.trim().parse::<i32>()
            {
                return gpid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the fake should spawn a grandchild");
    assert!(
        remuda_driver::shell_pty::lifecycle::process_alive(grandchild),
        "grandchild {grandchild} should be running before close"
    );

    // Harness budget: 3s, split 1.5s / 1.5s between the stdin-EOF and SIGTERM
    // slices, plus a fixed 2s reap after SIGKILL.
    const BUDGET: Duration = Duration::from_secs(3);
    const SLICE: Duration = Duration::from_millis(1500);
    let started = std::time::Instant::now();
    let closed = tokio::time::timeout(Duration::from_secs(20), driver.close()).await;
    let elapsed = started.elapsed();
    assert!(
        closed.is_ok(),
        "close did not return for a SIGTERM-proof child"
    );
    closed.unwrap().expect("close ack");

    // The child must have outlived BOTH bounded slices. If SIGTERM had killed it
    // (the wrong-thread-mask bug), close would have returned after one slice.
    // Tokio deadlines fire at or after their instant, so a real two-slice wait is
    // never shorter than the sum; 100ms of slack covers scheduling, not a whole
    // missing 1.5s slice.
    assert!(
        elapsed >= SLICE + SLICE - Duration::from_millis(100),
        "close returned in {elapsed:?}: the child did not survive the SIGTERM \
         slice, so rung 3 was never exercised (budget {BUDGET:?})"
    );
    assert!(
        elapsed < BUDGET + Duration::from_secs(3),
        "close took {elapsed:?}: the ladder is not bounded"
    );
    assert!(
        !process_alive(&pid),
        "pid {pid} survived a close that had to reach SIGKILL"
    );

    // The grandchild is in the child's group and is not the direct child, so
    // only rung 3's group SIGKILL could reap it. Give the kernel a beat, then
    // assert it is gone. Deleting the group signal leaves this one alive.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !remuda_driver::shell_pty::lifecycle::process_alive(grandchild),
        "grandchild {grandchild} survived: rung 3 did not SIGKILL the process group"
    );
    let _ = std::fs::remove_file(&grandchild_pid_file);
}

/// Item 2: the stdin-EOF request itself must be bounded.
///
/// When the child stops draining stdin, the driver's writer task blocks on the
/// pipe and its 64-slot channels fill — so `close_stdin`'s own enqueue blocks,
/// not just the subsequent `wait`. The old code awaited `close_stdin` *outside*
/// every timeout, which hung `instance.close` exactly the way the ladder was
/// meant to prevent. Here the fake stops reading after the handshake, the test
/// pumps prompts until the driver applies backpressure (channels saturated), and
/// close must still return inside its bound.
#[tokio::test]
async fn close_returns_when_stdin_is_not_drained_and_the_writer_is_saturated() {
    let mut env = BTreeMap::new();
    env.insert("FAKE_CLAUDE_STOP_READING".into(), "1".into());
    let (_tmp, driver, spec) = driver_with_env(ScriptKind::Ok, env);
    let _handle = driver.start(spec).await.expect("start");

    // Pump large prompts until enqueueing applies backpressure. One prompt is far
    // bigger than a pipe buffer, so the writer task blocks on the very first
    // write and the two 64-slot channels fill after a bounded number of sends;
    // anything after that parks until the child drains, which it never does.
    let saturated = async {
        let big = "x".repeat(256 * 1024);
        for _ in 0..2_000 {
            let result =
                tokio::time::timeout(Duration::from_millis(300), driver.send(prompt(&big))).await;
            if result.is_err() {
                return; // backpressure: the writer path is saturated
            }
        }
        panic!("never reached backpressure: the writer was not saturated");
    };
    tokio::time::timeout(Duration::from_secs(10), saturated)
        .await
        .expect("writer did not saturate within the window");

    // Without the bounded close_stdin this never returns; with it, rung 1 times
    // out (EOF never enqueued), SIGTERM at rung 2 reaps the non-reading child.
    let started = std::time::Instant::now();
    let closed = tokio::time::timeout(Duration::from_secs(20), driver.close()).await;
    assert!(closed.is_ok(), "close hung with a saturated writer");
    closed.unwrap().expect("close ack");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "close took {:?}: close_stdin was not bounded",
        started.elapsed()
    );
}

/// Item 4: a reader from a previous launch must never emit into the next one.
///
/// `close` joins or aborts the reader before it returns, and `start` re-arms
/// `exit_emitted`. Two sequential launch/close cycles on one driver must each
/// deliver exactly one `exited`, with no stale `exited` from launch A leaking
/// into launch B's channel.
#[tokio::test]
async fn relaunch_after_close_emits_one_exited_per_launch() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::Ok);

    for turn in ["first", "second"] {
        let mut handle = driver.start(spec.clone()).await.expect("start");
        driver.send(prompt(turn)).await.expect("send");
        let _ = collect_until(&mut handle, Duration::from_secs(5), |obs| {
            turn_done_count(obs) >= 1
        })
        .await;
        driver.close().await.expect("close");

        let mut exits = 0;
        while let Ok(Some(obs)) =
            tokio::time::timeout(Duration::from_millis(300), handle.recv()).await
        {
            if lifecycle_named(&obs) == Some("session") && lifecycle_status(&obs) == Some("exited")
            {
                exits += 1;
            }
        }
        assert_eq!(
            exits, 1,
            "launch for {turn:?} produced {exits} `exited` lifecycles; \
             a reader from a prior launch leaked into this channel"
        );
    }
}
