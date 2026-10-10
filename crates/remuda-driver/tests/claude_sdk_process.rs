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
    prompt_id(text, "msg-1")
}

fn prompt_id(text: &str, native_client_message_id: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: native_client_message_id.into(),
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

fn turn_started_count(obs: &[Observation]) -> usize {
    obs.iter()
        .filter(|o| lifecycle_named(o) == Some("turn_started"))
        .count()
}

/// Index of the first observation matching `pred`.
fn first_where(obs: &[Observation], pred: impl Fn(&Observation) -> bool) -> usize {
    obs.iter()
        .position(pred)
        .unwrap_or_else(|| panic!("no match in {} observations", obs.len()))
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

    // The session lifecycle from `system/init`, the `turn_started` emitted
    // right after the user frame is written, the assembled assistant message,
    // and the turn's settled `result` + usage are all present. The mpsc pump
    // buffers observations, so delivery order between session/message and the
    // post-write turn_started is not asserted; the causal relationship that
    // matters for the working→idle projection is turn_started before result.
    let session_at = events
        .iter()
        .position(|o| {
            lifecycle_named(o) == Some("session") && lifecycle_status(o) == Some("started")
        })
        .expect("session started lifecycle");
    let turn_started_at = events
        .iter()
        .position(|o| {
            lifecycle_named(o) == Some("turn_started") && lifecycle_status(o) == Some("working")
        })
        .expect("turn_started/working after the user frame");
    let message_at = events
        .iter()
        .position(|o| matches!(o.body, ObservationPayload::Message(_)))
        .expect("assistant message");
    let result_at = events
        .iter()
        .position(|o| lifecycle_status(o) == Some("turn_done"))
        .expect("turn_done");
    assert!(
        session_at < result_at && message_at < result_at && turn_started_at < result_at,
        "session/message/turn_started must all precede the result: session={session_at} \
         start={turn_started_at} message={message_at} result={result_at}"
    );
    // The turn_started event carries the native client message id so a later
    // fold can join the command to the turn (D-057 ma-sdk-state).
    use remuda_protocol::LifecyclePayload as LP;
    let started = events
        .iter()
        .find(|o| lifecycle_named(o) == Some("turn_started"))
        .expect("turn_started observation");
    if let ObservationPayload::Lifecycle(lp) = &started.body
        && let LP::Native(native) = lp.as_ref()
    {
        assert!(
            native.related_ids.contains_key("nativeClientMessageId"),
            "turn_started must carry the native client message id"
        );
    }
    // usage rides the terminal result (§2.5, `usage_from_result`).
    let usage_at = events
        .iter()
        .position(|o| matches!(o.body, ObservationPayload::Usage(_)))
        .expect("usage from result");
    assert!(usage_at > message_at, "usage must follow the message");

    driver.close().await.expect("close");
}

/// A settled result `error` (an API error such as 429) maps to a
/// `turn/result` lifecycle at status `error` with the native text on a
/// `lastError` related id and severity info — turn-level, never process
/// failure (D-057 OA6, ma-sdk-state).
#[tokio::test]
async fn an_error_result_carries_turn_level_last_error_at_info_severity() {
    let body = r#"{"type":"assistant","message":{"id":"msg_err","type":"message","role":"assistant","model":"fake","content":[{"type":"text","text":"retry please"}]},"session_id":"__SESSION__","uuid":"aaaaaaaa-0000-4000-8000-aaaaaaaaaaa1"}
{"type":"result","subtype":"success","is_error":true,"duration_ms":1,"num_turns":1,"result":"API Error: 429 try again","stop_reason":"end_turn","total_cost_usd":0,"usage":{"input_tokens":1,"output_tokens":1},"modelUsage":{},"permission_denials":[],"session_id":"__SESSION__","uuid":"aaaaaaaa-0000-4000-8000-aaaaaaaaaaa2","result_index":1,"queued_turn_count":0}
{"turn":"end"}
"#;
    let script_path = std::env::temp_dir().join(format!(
        "ma-sdk-state-error-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    tokio::fs::write(&script_path, body)
        .await
        .expect("write script");

    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_CLAUDE_SCRIPT".to_owned(),
        script_path.to_string_lossy().into_owned(),
    );
    let (_tmp, driver, spec) = driver_with_env(ScriptKind::Ok, env);
    let mut handle = driver.start(spec).await.expect("start");
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);
    driver.send(prompt("one")).await.expect("send");
    let events = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        obs.iter()
            .any(|o| lifecycle_named(o) == Some("result") && lifecycle_status(o) == Some("error"))
    })
    .await;

    use remuda_protocol::{LifecyclePayload as LP, Severity};
    let error_result = events
        .iter()
        .find(|o| lifecycle_named(o) == Some("result") && lifecycle_status(o) == Some("error"))
        .expect("turn/result error");
    let native = match &error_result.body {
        ObservationPayload::Lifecycle(lp) => match lp.as_ref() {
            LP::Native(native) => native,
            other => panic!("expected native lifecycle, got {other:?}"),
        },
        other => panic!("expected lifecycle, got {other:?}"),
    };
    assert_eq!(native.topic, remuda_protocol::LifecycleTopic::Turn);
    assert_eq!(
        native.severity,
        Severity::Info,
        "a turn error is not an error-severity event"
    );
    assert_eq!(
        native.related_ids.get("lastError").map(String::as_str),
        Some("API Error: 429 try again"),
        "native error text is carried for the additive turn-error marker"
    );
    assert_eq!(
        native
            .related_ids
            .get("queuedTurnCount")
            .map(String::as_str),
        Some("0")
    );

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

    // The driver's two SIGINT/SIGHUP slices are 2 s each in production; the
    // assertions below only need the child to have survived *both* (so rung 3
    // is the thing that killed it), hence the 1.5 s/3 s literals.
    const SLICE: Duration = Duration::from_millis(1500);
    let started = std::time::Instant::now();
    // Loaded-host ceiling: 2 s + 2 s signal rungs, the 0.5 s SIGKILL window
    // and the driver's 30 s post-SIGKILL reap grace (REAP_EXITING_GRACE).
    let closed = tokio::time::timeout(Duration::from_secs(60), driver.close()).await;
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
         slice, so rung 3 was never exercised"
    );
    assert!(
        elapsed < Duration::from_secs(36),
        "close took {elapsed:?}: the ladder is not bounded by the configured rungs"
    );

    // Wait on the observable state, not on a fixed sleep: on the loaded gate
    // host a SIGKILLed child can take seconds to run the kernel exit path and
    // become a zombie its parent has reaped, and a zombie still answers
    // signal 0. The driver's post-SIGKILL reap grace is 30 s; use the same
    // bound here.
    async fn wait_gone(pid: i32, what: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if !remuda_driver::shell_pty::lifecycle::process_alive(pid) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{what} {pid} survived a close that had to reach SIGKILL"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    wait_gone(pid.parse().expect("numeric pid"), "pid").await;

    // The grandchild is in the child's group and is not the direct child, so
    // only rung 3's group SIGKILL could reap it. Wait for it to be gone the
    // same way; deleting the group signal leaves this one alive.
    wait_gone(grandchild, "grandchild").await;
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

/// r3 item 5 + ma-sdk-state r5 item 1: a blocked SEND cannot gate close, and
/// the child's stdout must keep being DRAINED under bidirectional pipe
/// pressure.
///
/// The fake stops reading stdin AND floods stdout forever. A prompt bigger
/// than a pipe buffer blocks in the writer task. Pre-r3 `send` held the
/// `live` (and turn-order) lock across that write, so a concurrent `close`
/// could not start its ladder and the process hung. r5 parks the publication
/// worker at the turn's in-queue reservation ticket while the write is
/// blocked: the reader task keeps draining the child's stdout into the
/// unbounded queue (so the OS pipe never fills and close is not gated), but
/// frames behind the ticket are not EMITTED until the write resolves. Here we:
///  1. start a large send and prove it is parked on the blocked pipe;
///  2. prove emission is gated at the ticket while the reader still drains
///     (no pipe deadlock);
///  3. close concurrently — it must reach the ladder, reap the child within
///     its bound, and release the parked send (with an error, not a success).
#[tokio::test]
async fn a_blocked_send_cannot_gate_close_and_emission_gates_at_the_ticket_without_deadlock() {
    let mut env = BTreeMap::new();
    env.insert("FAKE_CLAUDE_STOP_READING".into(), "1".into());
    env.insert("FAKE_CLAUDE_STDOUT_FLOOD".into(), "1".into());
    let (_tmp, driver, spec) = driver_with_env(ScriptKind::Ok, env);
    let mut handle = driver.start(spec).await.expect("start");

    // 1. A large prompt parks on the blocked pipe (write never completes).
    let big = "x".repeat(256 * 1024);
    let mut send_fut = Box::pin(driver.send(prompt(&big)));
    let parked = tokio::time::timeout(Duration::from_millis(800), &mut send_fut).await;
    assert!(
        parked.is_err(),
        "the send must stay parked while the child does not drain stdin"
    );

    // 2. The flood (~one frame every 500us, i.e. thousands over the window if
    // it flowed) must NOT be emitted while the worker holds the reservation
    // ticket. Only the few frames enqueued ahead of the ticket can be
    // delivered before the worker parks; a free-running drain would deliver
    // orders of magnitude more. The reader still drains the child's stdout
    // into the unbounded queue, so the OS pipe never fills (no close deadlock).
    let mut emitted = 0usize;
    let gate_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < gate_deadline {
        if let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), handle.recv()).await {
            emitted += 1;
        }
    }
    assert!(
        emitted < 100,
        "frames behind the parked reservation must not be emitted; got {emitted} \
         (a free-running 2 kHz flood delivers thousands over the window)"
    );

    // 3. close must start immediately (no `live` held by the blocked send),
    // bound the unresponsive child away, and unblock the send.
    let started = std::time::Instant::now();
    let close_fut = tokio::time::timeout(Duration::from_secs(20), driver.close());
    let (send_result, closed) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(20), &mut send_fut),
        close_fut
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "close took {:?}: a blocked send gated the close ladder",
        started.elapsed()
    );
    closed.expect("close timed out").expect("close ack");
    let send_result = send_result.expect("the parked send never resolved after close");
    assert!(
        send_result.is_err(),
        "a write the dead child never drained must surface an error, not success: {send_result:?}"
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

// The deterministic write-window barrier test
// (`a_result_queued_during_the_write_window_starts_then_results`, below)
// replaces the old live-only version of this assertion: it forces the exact
// start-before-result ordering, including the result arriving while the
// prompt write is still in flight.

/// D-057 OA6 r2 item 3: `send` resolves only after the user line is WRITTEN,
/// and propagates the write failure once the child's stdin is gone (closed
/// pipe / broken pipe) instead of reporting `transport_written` for a prompt
/// that never reached the process.
#[tokio::test]
async fn a_send_after_the_child_stdin_closed_is_an_error_not_written() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::Ok);
    let mut handle = driver.start(spec).await.expect("start");
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);

    // Drain the first turn so the child is up and reading.
    driver.send(prompt("one")).await.expect("first send ok");
    let _ = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 1
    })
    .await;

    // Close the child's stdin and reap the process; the writer channel then
    // rejects the next write.
    driver.close().await.expect("close");

    let result = driver.send(prompt("after close")).await;
    assert!(
        result.is_err(),
        "a prompt after stdin closed must propagate the write failure, got {result:?}"
    );
}

/// ma-sdk-state r4 item 1 (barrier): the write has acked and the send task is
/// parked BEFORE it commits the reservation/enqueues turn_started, while the
/// child's result is already mapped and queued in the publication worker. The
/// reserved turn's start must still be published before ITS result.
#[tokio::test]
async fn a_result_queued_during_the_write_window_starts_then_results() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::Ok);
    let mut handle = driver.start(spec).await.expect("start");
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);

    let barrier = driver.arm_write_commit_barrier();
    let send_task = tokio::spawn(async move {
        let result = driver.send(prompt("race prompt")).await;
        (driver, result)
    });

    // Provably occupy the race window: write acked, result queued in the
    // worker, turn_started not yet enqueued.
    barrier
        .wait_until_result_parked(Duration::from_secs(5))
        .await;
    barrier.release();

    let (driver, result) = send_task.await.expect("send task");
    result.expect("the write acked, so the send resolves Ok");

    let all = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_started_count(obs) >= 1 && turn_done_count(obs) >= 1
    })
    .await;
    assert_eq!(
        turn_started_count(&all),
        1,
        "exactly one turn_started even under the forced race: {all:?}"
    );
    let start = first_where(&all, |o| lifecycle_named(o) == Some("turn_started"));
    let done = first_where(&all, |o| lifecycle_status(o) == Some("turn_done"));
    assert!(
        start < done,
        "turn_started ({start}) must precede its own result ({done})"
    );

    driver.close().await.expect("close");
}

/// ma-sdk-state r4 item 3 (OA6): the fake child closes its OWN stdin fd after
/// the handshake and keeps running with stdout open. The acknowledged write
/// must surface as an error, but the process is still alive:
/// * `send` returns `Err`, never `transport_written`;
/// * the reservation produced ZERO turn_started observations;
/// * the child process survives (parent-death reaper notwithstanding) and
///   `process_gone()` stays false — the Node must not terminalize on this.
#[tokio::test]
async fn a_live_child_that_closed_its_stdin_errors_the_send_without_any_turn_start() {
    let (_tmp, driver, spec) = driver_with_env(
        ScriptKind::Ok,
        BTreeMap::from([("FAKE_CLAUDE_CLOSE_STDIN".into(), "1".into())]),
    );
    let mut handle = driver.start(spec).await.expect("start");
    let pid = handle
        .ack()
        .native_ids
        .get("pid")
        .expect("pid on the launch ack")
        .clone();
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);

    // Give the child a beat to run its post-handshake stdin close.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        process_alive(&pid),
        "the child parked alive with stdin closed"
    );

    let result = driver.send(prompt("into a closed stdin")).await;
    assert!(
        result.is_err(),
        "the write against the child-closed stdin must error, got {result:?}"
    );
    assert!(
        !driver.process_gone().await,
        "a closed stdin on a live child is not process loss"
    );

    // Drain for a grace period: the failed reservation must not have leaked a
    // turn_started, and the live child sends no stdout exit either.
    let mut leaked = Vec::new();
    while let Ok(Some(obs)) = tokio::time::timeout(Duration::from_millis(300), handle.recv()).await
    {
        leaked.push(obs);
    }
    for obs in &leaked {
        assert_ne!(
            lifecycle_named(obs),
            Some("turn_started"),
            "a failed write emits no turn_started: {obs:?}"
        );
        assert!(
            !(lifecycle_named(obs) == Some("session") && lifecycle_status(obs) == Some("exited")),
            "a live child emits no exit while its stdin is closed: {obs:?}"
        );
    }
    assert!(
        process_alive(&pid),
        "the child is still running after the failed write"
    );

    driver.close().await.expect("close");
    assert!(
        !process_alive(&pid),
        "close still reaps the live child through the ladder"
    );
}

/// ma-sdk-state r4 item 4: when the child floods stdout and nobody drains
/// the event channel, `close` aborts the blocked publication worker — yet
/// the terminal session-exit must still be delivered on a reserved channel
/// slot, and it must be exactly ONE, ordered after every buffered frame and
/// before stream EOF. Without the reserved permit the exit was dropped when
/// the worker was aborted (events sender taken), so a later drain saw
/// buffered frames then EOF with no exit.
#[tokio::test]
async fn close_under_output_pressure_still_delivers_exactly_one_terminal_exit() {
    let mut env = BTreeMap::new();
    env.insert("FAKE_CLAUDE_STDOUT_FLOOD".into(), "1".into());
    let (_tmp, driver, spec) = driver_with_env(ScriptKind::Ok, env);
    let mut handle = driver.start(spec).await.expect("start");

    // Do NOT drain handle at all. Wait until the 255-slot observation channel
    // is saturated (the publication worker is parked on emit). The child
    // floods fast; give the saturation a bounded window.
    let saturated = async {
        for _ in 0..200 {
            if driver.publication_is_saturated().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("publication never saturated with a flooding child");
    };
    tokio::time::timeout(Duration::from_secs(10), saturated)
        .await
        .expect("saturation window");

    // Close under pressure: it aborts the blocked worker and reaps the child
    // through its bounded ladder, and must not hang.
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(20), driver.close())
        .await
        .expect("close hung under output pressure")
        .expect("close ack");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "close took {:?} under output pressure",
        started.elapsed()
    );

    // Now drain everything until EOF.
    let mut terminal = Vec::new();
    while let Some(obs) = handle.recv().await {
        if lifecycle_named(&obs) == Some("session") && lifecycle_status(&obs) == Some("exited") {
            terminal.push(obs);
        }
    }
    assert_eq!(
        terminal.len(),
        1,
        "exactly one terminal session/exited event survives the saturated \
         channel + aborted worker"
    );
}

/// ma-sdk-state r4 item 5(c): a host permission response must ride the SAME
/// FIFO stdin queue as prompts. It used to go through the inbound bridge
/// (pub_in_tx -> a forward task -> writer_tx), so a prompt enqueued
/// afterwards could be written first. Record the child's actual stdin order
/// and prove the `control_response` precedes the following `user` frame.
#[tokio::test]
async fn a_permission_response_is_written_before_a_later_prompt_on_one_fifo() {
    let order = std::env::temp_dir().join(format!(
        "sdk-fifo-{}.txt",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (_tmp, driver, spec) = driver_with_env(
        ScriptKind::Approval,
        BTreeMap::from([(
            "FAKE_CLAUDE_STDIN_FILE".into(),
            order.to_string_lossy().into_owned(),
        )]),
    );
    let mut handle = driver.start(spec).await.expect("start");
    driver.send(prompt("touch")).await.expect("send");
    let events = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        obs.iter()
            .any(|o| matches!(o.body, ObservationPayload::InteractionRequested(_)))
    })
    .await;

    let interaction = events
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::InteractionRequested(payload) => Some(payload.interaction.clone()),
            _ => None,
        })
        .expect("approval interaction");

    // Respond to the approval, then enqueue ANOTHER prompt. Both now go
    // through the single ProcessWriter FIFO.
    driver
        .respond_interaction(
            interaction.meta.id.clone(),
            remuda_protocol::InteractionAnswer::Approval(Box::new(
                remuda_protocol::ApprovalAnswer {
                    option_id: "allow".into(),
                    input_digest: {
                        use remuda_protocol::InteractionRequest;
                        match interaction.request.clone() {
                            InteractionRequest::Approval(req) => req.input_digest,
                            _ => dummy_digest(),
                        }
                    },
                },
            )),
        )
        .await
        .expect("respond");
    // Distinct client id so the second prompt is not deduped.
    let mut second = prompt("after approval");
    if let DriverInput::Prompt(p) = &mut second {
        p.native_client_message_id = "msg-2".into();
    }
    driver.send(second).await.expect("second send");

    // Wait for the writer to flush and the fake to read both frames. Poll
    // until both markers appear (the first `user` is only recorded when the
    // fake plays the turn; the control_response marker is recorded inside
    // the fake's wait loop).
    let text = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let text = std::fs::read_to_string(&order).unwrap_or_default();
            let lines = text.lines().collect::<Vec<_>>();
            if lines.contains(&"control_response")
                && lines.iter().filter(|l| **l == "user").count() >= 2
            {
                break text;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("both frames never recorded");
    let markers: Vec<String> = text.lines().map(str::to_owned).collect();
    let response_at = markers.iter().position(|l| l == "control_response");
    let user_at = markers.iter().rposition(|l| l == "user");
    assert!(
        response_at < user_at,
        "permission response must precede the later prompt on the wire: {markers:?}"
    );

    driver.close().await.expect("close");
}

// NOTE ma-sdk-state r5 item 6 (5a): the former r4 5a test here ran the two
// prompts SEQUENTIALLY (A fully drained before B), so it never armed the
// item-1 barrier and could not observe a buffered result. Its per-turn
// attribution claim now lives in the node barrier suite, which forces A's
// result to be buffered while B is published and folds the stream through
// engine_turn_activity AND the Hub derive:
// `buffered_turn_a_result_keeps_the_row_working_until_turn_b_settles` in
// crates/remuda-node/tests/sdk_turn_publication_barrier.rs. Plain sequential
// two-turn coverage remains in `the_sdk_child_survives_a_second_prompt`.
/// ma-sdk-state r5 item 1 (replaces the old hand-mapped
/// `an_older_buffered_result_cannot_settle_a_newer_outstanding_input`): drive
/// two turns through the REAL publication task with turn A's ticket parked
/// while B is registered, so B's book is open before the worker maps A's
/// buffered result. Two invariants are checked on the published stream:
/// (1) ORDER — start_A < result_A < start_B < result_B, so no result ever
/// precedes its own turn_started (the in-queue ticket makes this structural);
/// (2) ATTRIBUTION — A's result is mapped while turn B is outstanding, so it
/// carries no settledRootTurn and cannot settle the root; only B's result
/// settles. The raw turn lifecycle still goes working → turn_done → working →
/// turn_done, and the child survives.
#[tokio::test]
async fn an_older_buffered_result_cannot_settle_a_newer_outstanding_input_through_the_publication_task()
 {
    let (_tmp, driver, spec) = driver_for(ScriptKind::TwoTurn);
    let mut handle = driver.start(spec).await.expect("start");
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);

    // Arm the per-driver barrier BEFORE moving the driver into the send task.
    let barrier = driver.arm_write_commit_barrier();
    // One task owns the driver: send A (which commits while the worker is held
    // at its ticket) then send B, whose book is opened before the worker maps
    // A's buffered result.
    let send_both = tokio::spawn(async move {
        driver.send(prompt_id("first", "msg-a")).await?;
        driver.send(prompt_id("second", "msg-b")).await?;
        Ok::<ClaudeSdkDriver, DriverError>(driver)
    });
    // Wait until the worker holds turn A's ticket, the write has acked, AND
    // turn A's RESULT frame is queued behind the ticket. Waiting for the result
    // specifically (not just any frame) is what makes the strict cross-turn
    // order deterministic rather than scheduler luck (r6 item 3): A's result is
    // in the queue BEFORE B even begins.
    barrier
        .wait_until_result_parked(Duration::from_secs(5))
        .await;
    // Release the SEND only: A commits, the worker publishes start_A then
    // stays parked at the ticket (worker gate still shut). A's result is
    // already queued, so it precedes B's start.
    barrier.release_send();
    // Wait until B's reservation is enqueued — B's book is now open, in the
    // queue behind A's buffered result.
    barrier
        .wait_until_reserves_enqueued(2, Duration::from_secs(5))
        .await;
    // Now let the worker map. It maps A's result with turn B outstanding.
    barrier.resume_worker();
    let send_both = send_both.await.expect("send task");
    let driver = send_both.as_ref().expect("both prompt writes ack");

    // Drain BOTH turns in one pass — no draining between the two sends, so this
    // is the unfiltered publication order.
    let all = collect_until(&mut handle, Duration::from_secs(8), |obs| {
        let starts = obs
            .iter()
            .filter(|o| lifecycle_named(o) == Some("turn_started"))
            .count();
        starts >= 2 && turn_done_count(obs) >= 2
    })
    .await;

    let positions = |name: &str| {
        all.iter()
            .enumerate()
            .filter_map(|(i, o)| (lifecycle_named(o) == Some(name)).then_some(i))
            .collect::<Vec<_>>()
    };
    let starts = positions("turn_started");
    let results = positions("result");
    assert_eq!(starts.len(), 2, "two turn_started: {all:?}");
    assert_eq!(results.len(), 2, "two results: {all:?}");
    // The invariant: each turn's start precedes its OWN result, and the older
    // turn's result is fully published BEFORE the newer turn starts.
    assert!(
        starts[0] < results[0] && results[0] < starts[1] && starts[1] < results[1],
        "expected start_A < result_A < start_B < result_B, got starts={starts:?} results={results:?}"
    );

    // Per-turn attribution (the claim the old hand-mapped test made): A's
    // result is mapped while B's turn is already open, so it must NOT carry
    // settledRootTurn — it cannot settle the root a newer turn owns. Only B's
    // result, with no turn outstanding behind it, settles.
    let settles_at = |pos: usize| {
        matches!(&all[pos].body,
            ObservationPayload::Lifecycle(p) if matches!(p.as_ref(),
                LifecyclePayload::Native(n) if n.native_name == "result"
                    && n.related_ids.get("settledRootTurn").map(String::as_str) == Some("true")))
    };
    assert!(
        !settles_at(results[0]),
        "the older buffered result A must not settle while turn B is outstanding"
    );
    assert!(
        settles_at(results[1]),
        "the last outstanding turn B's result settles the root"
    );

    // Raw turn lifecycle still runs working → turn_done → working → turn_done:
    // A's end-of-turn result is published (turn_done) without idling the root,
    // and B re-enters working at its own start. (The Node engine keeps the row
    // working through A because A's result lacks settledRootTurn, and derives
    // idle only from B's settling result.)
    let mut edges: Vec<&str> = Vec::new();
    for status in all.iter().filter_map(lifecycle_status) {
        if edges.last() != Some(&status) && matches!(status, "working" | "turn_done") {
            edges.push(status);
        }
    }
    assert_eq!(
        edges,
        vec!["working", "turn_done", "working", "turn_done"],
        "per-turn working/turn_done edges: {edges:?}"
    );

    let pid = handle.ack().native_ids.get("pid").cloned().expect("pid");
    assert!(process_alive(&pid), "the sdk child survives both turns");
    driver.close().await.expect("close");
}

/// ma-sdk-state r5 item 4: once the sdk child has really exited (the reader
/// emitted its terminal lifecycle), `process_gone` through the `Driver` trait
/// must report true. Before forwarding, `ClaudeSdkDriver` fell back to the
/// trait default (always false), so the inner claude-print evidence was
/// unreachable and a failed write to a dead child was misread as a control
/// error.
#[tokio::test]
async fn process_gone_is_true_through_the_driver_trait_after_the_child_exits() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::Ok);
    let mut handle = driver.start(spec).await.expect("start");
    let as_driver: &dyn Driver = &driver;
    assert!(
        !as_driver.process_gone().await,
        "a freshly launched live child is not gone"
    );
    driver.send(prompt("hi")).await.expect("send");
    let _ = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 1
    })
    .await;

    driver.close().await.expect("close");
    // Drain the terminal exit so the reader flips exit_emitted.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw_exit = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), handle.recv()).await {
            Ok(Some(obs)) => {
                if lifecycle_named(&obs) == Some("session")
                    && lifecycle_status(&obs) == Some("exited")
                {
                    saw_exit = true;
                }
            }
            _ => break,
        }
    }
    assert!(saw_exit, "the child emitted one session exited lifecycle");
    assert!(
        as_driver.process_gone().await,
        "process_gone must forward the inner driver's end evidence"
    );
}

/// ma-sdk-state r6 item 1: a prompt whose staged image cannot be read returns
/// Err from content resolution BEFORE any turn book/ticket exists. The next
/// normal prompt must still publish turn_started and its result — the failed
/// (or here, never-opened) reservation must neither park the publication
/// worker nor leave an empty outstanding turn that blocks the later settle.
#[tokio::test]
async fn a_missing_image_send_errors_and_the_next_turn_still_starts_and_results() {
    let (_tmp, driver, spec) = driver_for(ScriptKind::Ok);
    let mut handle = driver.start(spec).await.expect("start");
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);

    let id = remuda_protocol::Id::new("obj").expect("object id");
    let missing_image = DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![
            ContentBlock::Image(Box::new(remuda_protocol::MediaBlock {
                object_id: id.clone(),
                media_type: "image/png".into(),
                name: Some("shot.png".into()),
                anchor: None,
                size: None,
            })),
            ContentBlock::Resource(Box::new(remuda_protocol::ResourceBlock {
                uri: "file:///nonexistent/r6-missing-shot.png".into(),
                media_type: Knowledge::Known {
                    value: "image/png".into(),
                },
                object_id: Some(id),
            })),
            ContentBlock::Text(Box::new(TextBlock {
                text: "describe the missing image".into(),
            })),
        ],
        origin: InputOrigin::Human,
        native_client_message_id: "msg-r6-missing".into(),
    }));
    let failed = driver.send(missing_image).await;
    assert!(
        failed.is_err(),
        "an unreadable staged image must fail the send: {failed:?}"
    );

    // The very next prompt goes through the full publication pipeline.
    driver
        .send(prompt("after the failed image"))
        .await
        .expect("the next prompt must still send after a content-resolution failure");
    let obs = collect_until(&mut handle, Duration::from_secs(5), |seen| {
        seen.iter()
            .any(|o| lifecycle_named(o) == Some("turn_started"))
            && turn_done_count(seen) >= 1
    })
    .await;
    assert!(
        obs.iter()
            .any(|o| lifecycle_named(o) == Some("turn_started")),
        "turn_started is still published after the failed image send"
    );
    assert_eq!(
        turn_started_count(&obs),
        1,
        "the failed image send leaked no turn_started"
    );
    assert!(turn_done_count(&obs) >= 1, "the result is still published");

    driver.close().await.expect("close");
}
