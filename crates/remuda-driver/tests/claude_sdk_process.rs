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
use remuda_testing::{ScriptKind, ensure_workspace_bin, fake_claude_argv, script_path};
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
    let mut spec = load_spec();
    spec.cwd = tmp
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    (tmp, ClaudeSdkDriver::new(options), spec)
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

/// Golden argv. `-p` is the whole difference between the carriers, and its
/// absence is what lets stdin stay open (§2.1, key decision 3).
#[test]
fn sdk_argv_has_no_dash_p() {
    let argv = fake_claude_argv("11111111-1111-4111-8111-111111111111", false);
    assert!(
        !argv.iter().any(|a| a == "-p" || a == "--print"),
        "sdk argv must not print-and-exit: {argv:?}"
    );
    for flag in ["--input-format", "--output-format", "stream-json"] {
        assert!(argv.iter().any(|a| a == flag), "missing {flag} in {argv:?}");
    }
    // §2.1: never these, on any template.
    for flag in [
        "--bare",
        "--safe-mode",
        "--no-session-persistence",
        "--continue",
    ] {
        assert!(!argv.iter().any(|a| a == flag), "sdk argv has {flag}");
    }
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

    // Same process: `capabilities` reads the live recipe, and the ack pid was
    // minted once at launch. A relaunch would have replaced both.
    let live_pin = driver.capabilities().await.expect("capabilities");
    assert_eq!(live_pin.driver_kind, DriverKind::ClaudeSdk);
    assert_eq!(
        handle.ack().native_ids.get("pid"),
        Some(&pid),
        "pid changed: the second turn went to a different process"
    );

    driver.close().await.expect("close");
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
