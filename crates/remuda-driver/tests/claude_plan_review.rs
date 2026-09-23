//! Real-condition chain tests for D-051 plan review, driven by a sanitized
//! NDJSON VCR peer (`fake-claude-replay`) that replays recorded
//! `claude -p` stream-json sessions (Claude 2.1.277, plan mode, host
//! `can_use_tool`). See crates/remuda-testing/fixtures/SOURCES.md.
//!
//! The child's ExitPlanMode pause is minted by the driver as a PlanReview
//! (not an Approval). Answering it through the driver produces the exact
//! `behavior`/`message` control response the recorded CLI got:
//!
//! * approve → "User has approved your plan" tool_result, then the planned
//!   Bash really runs;
//! * deny + feedback → that feedback verbatim as an `is_error` tool_result,
//!   no Bash.
//!
//! The VCR exits non-zero on a verdict mismatch, so answering deny to the
//! allow fixture proves the peer really checks the verdict.

use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use remuda_driver::{
    BinaryPin, BinarySource, Delegation, Driver, ProviderHealth, ProviderKind, ProviderProfile,
    SecretRef,
};
use remuda_protocol::{
    ClaudeInteractionMode, ClaudePermission, ClaudePermissionMode, ContentBlock, Digest,
    DriverInput, InputOrigin, InstanceSpec, InteractionAnswer, InteractionKind, InteractionRequest,
    Knowledge, Observation, ObservationPayload, PermissionMode, PlanReviewAnswer, PromptInput,
    PromptMode, TextBlock,
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;

/// Exit the plan-mode peer with this decision.
enum Decision {
    Approve,
    Deny(&'static str),
}

const ALLOW_PLAN: &str = "# Plan\n\n1. Run the shell command `echo plan-approved`.\n";
const DENY_PLAN: &str = "# Plan\n\n1. Run the shell command `echo plan-denied`.\n";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../remuda-testing/fixtures/claude")
        .join(name)
}

fn dummy_digest() -> Digest {
    Digest::try_from(format!("sha256:{:0>64}", "a")).unwrap()
}

fn sha256_hex(bytes: &[u8]) -> Digest {
    Digest::try_from(format!("sha256:{:x}", Sha256::digest(bytes))).unwrap()
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: "https://gateway.example".into(),
        delegation: Delegation::None,
        secret_ref: Some(SecretRef::parse("env:REMUDA_CLAUDE_PRINT_SECRET").unwrap()),
        models: vec!["test-model".into()],
        health: ProviderHealth::Healthy,
    }
}

fn replay_driver(fixture_name: &str) -> (TempDir, ClaudePrintDriver, InstanceSpec) {
    let path = remuda_testing::ensure_workspace_bin("fake-claude-replay");
    assert!(path.is_file(), "missing {}", path.display());
    let pin = BinaryPin {
        abs_path: path.to_string_lossy().into_owned(),
        version: "fake-claude-replay".into(),
        sha256: dummy_digest(),
    };
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let mut extra = BTreeMap::new();
    extra.insert(
        "FAKE_CLAUDE_FIXTURE".into(),
        fixture(fixture_name).to_string_lossy().into_owned(),
    );
    // Strict: the driver-driven replay verifies the whole permission frame and
    // normalizes only the generated initialize request id.
    extra.insert("FAKE_CLAUDE_STRICT".into(), "1".into());
    let mut options = ClaudePrintOptions::new(profile(), launch, home, BinarySource::Pinned(pin));
    options.origin = InputOrigin::Human;
    options.extra_env = extra;
    options.handshake_timeout = Duration::from_secs(10);
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.cwd = tmp
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::Plan,
        interaction: ClaudeInteractionMode::Host,
    }));
    // `tmp` is returned so its lifetime covers the whole driver run.
    (tmp, ClaudePrintDriver::new(options), spec)
}

/// Assert the child process the driver spawned actually exited with status 0 —
/// the real OS wait status the driver reaps in close(), not a file the peer
/// self-reported.
async fn assert_child_exited_cleanly(driver: &ClaudePrintDriver) {
    let status = driver
        .child_exit_status()
        .await
        .expect("driver reaped the replay child");
    assert!(
        status.success(),
        "fake-claude-replay exited non-zero (verdict/frame mismatch): {status}"
    );
}

fn prompt() -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: "Immediately call the ExitPlanMode tool.".into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "msg-1".into(),
    }))
}

/// Start the VCR driver and wait for the pending PlanReview; asserts the
/// inline plan text and digest against the plan this fixture records.
async fn start_and_expect_plan_review(
    driver: &ClaudePrintDriver,
    spec: InstanceSpec,
    expected_plan: &str,
) -> (
    remuda_driver::RunHandle,
    Vec<Observation>,
    remuda_protocol::Interaction,
) {
    let mut handle = driver.start(spec).await.expect("start");
    driver.send(prompt()).await.expect("send");
    let events = collect_until(&mut handle, Duration::from_secs(10), |obs| {
        obs.iter().any(|o| {
            matches!(
                &o.body,
                ObservationPayload::InteractionRequested(payload)
                    if payload.interaction.kind == InteractionKind::PlanReview
            )
        })
    })
    .await
    .expect("a plan review was not raised");
    let request = events
        .iter()
        .find_map(|obs| match &obs.body {
            ObservationPayload::InteractionRequested(payload)
                if payload.interaction.kind == InteractionKind::PlanReview =>
            {
                Some(payload.interaction.clone())
            }
            _ => None,
        })
        .expect("plan review");
    let InteractionRequest::PlanReview(review) = &request.request else {
        panic!("expected plan review request");
    };
    assert_eq!(review.plan.as_deref(), Some(expected_plan));
    assert_eq!(review.plan_digest, sha256_hex(expected_plan.as_bytes()));
    (handle, events, request)
}

async fn collect_until(
    handle: &mut remuda_driver::RunHandle,
    timeout: Duration,
    mut pred: impl FnMut(&[Observation]) -> bool,
) -> Option<Vec<Observation>> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, handle.recv()).await {
            Ok(Some(obs)) => {
                out.push(obs);
                if pred(&out) {
                    return Some(out);
                }
            }
            Ok(None) | Err(_) => return None,
        }
    }
    None
}

fn lifecycle_turn_done(events: &[Observation]) -> bool {
    events.iter().any(|o| match &o.body {
        ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
            remuda_protocol::LifecyclePayload::Native(native) => {
                matches!(&native.status, Knowledge::Known { value } if value == "turn_done")
            }
            _ => false,
        },
        _ => false,
    })
}

fn tool_result_texts(events: &[Observation]) -> Vec<String> {
    events
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::ToolResult(payload) => Some(
                payload
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect()
}

fn text_contains(blocks: &[ContentBlock], needle: &str) -> bool {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .any(|text| text.contains(needle))
}

fn bash_commands(events: &[Observation]) -> Vec<String> {
    // One tool call produces streaming (Unknown input) plus Open/Close (Known)
    // observations sharing its id; keep the first Known command per id only.
    let mut seen = std::collections::BTreeSet::new();
    events
        .iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::ToolCall(call)
                if matches!(&call.tool_name, Knowledge::Known { value } if value == "Bash")
                    && matches!(&call.input, Knowledge::Known { .. }) =>
            {
                let id = call.tool_call_id.to_string();
                if !seen.insert(id) {
                    return None;
                }
                if let Knowledge::Known { value } = &call.input {
                    value
                        .get("command")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect()
}

fn plan_answer(
    request: &remuda_protocol::Interaction,
    option: &str,
    feedback: Option<String>,
) -> InteractionAnswer {
    let InteractionRequest::PlanReview(review) = &request.request else {
        panic!("not a plan review");
    };
    InteractionAnswer::PlanReview(Box::new(PlanReviewAnswer {
        option_id: option.into(),
        plan_revision: review.plan_revision,
        plan_digest: review.plan_digest.clone(),
        feedback,
    }))
}

#[tokio::test]
async fn approving_the_plan_review_runs_the_planned_command() {
    let (_tmp, driver, spec) = replay_driver("claude-exit-plan-mode-allow.jsonl");
    let (mut handle, _, request) = start_and_expect_plan_review(&driver, spec, ALLOW_PLAN).await;

    driver
        .respond_interaction(
            request.meta.id.clone(),
            plan_answer(&request, "approve", None),
        )
        .await
        .expect("respond");

    let rest = collect_until(&mut handle, Duration::from_secs(10), lifecycle_turn_done)
        .await
        .expect("turn did not finish");

    assert!(
        rest.iter().any(|o| match &o.body {
            ObservationPayload::ToolResult(payload) => {
                text_contains(&payload.blocks, "User has approved your plan")
            }
            _ => false,
        }),
        "expected the plan-approved tool_result"
    );
    assert_eq!(bash_commands(&rest), vec!["echo plan-approved"]);
    assert!(
        tool_result_texts(&rest)
            .iter()
            .any(|t| t.contains("plan-approved")),
        "the planned Bash output must appear in its tool_result"
    );
    driver.close().await.expect("close");
    // The replay verified the whole allow frame (exact input, no
    // updatedPermissions) and its child exited cleanly (real OS status).
    assert_child_exited_cleanly(&driver).await;
}

#[tokio::test]
async fn denying_the_plan_review_feeds_feedback_to_the_model_and_blocks_execution() {
    let (_tmp, driver, spec) = replay_driver("claude-exit-plan-mode-deny.jsonl");
    let (mut handle, _, request) = start_and_expect_plan_review(&driver, spec, DENY_PLAN).await;

    driver
        .respond_interaction(
            request.meta.id.clone(),
            plan_answer(&request, "deny", Some("plan review denied".into())),
        )
        .await
        .expect("respond");

    let rest = collect_until(&mut handle, Duration::from_secs(10), lifecycle_turn_done)
        .await
        .expect("turn did not finish");

    assert!(
        rest.iter().any(|o| match &o.body {
            ObservationPayload::ToolResult(payload) => {
                text_contains(&payload.blocks, "plan review denied")
            }
            _ => false,
        }),
        "expected the deny feedback as the tool_result"
    );
    assert!(
        bash_commands(&rest).is_empty(),
        "no Bash may execute after deny"
    );
    assert!(
        !tool_result_texts(&rest)
            .iter()
            .any(|t| t.contains("plan-denied")),
        "the planned command output must not appear"
    );
    driver.close().await.expect("close");
    assert_child_exited_cleanly(&driver).await;
}

/// A wrong verdict must be caught by the VCR: deny against the ALLOW fixture
/// makes the peer exit non-zero, so no driver can claim a rejected plan was
/// approved.
#[test]
fn answering_deny_to_the_allow_fixture_makes_the_replay_exit_nonzero() {
    let binary = remuda_testing::ensure_workspace_bin("fake-claude-replay");
    let status = drive_vcr_with_verdict(
        &binary,
        fixture("claude-exit-plan-mode-allow.jsonl"),
        Decision::Deny("plan review denied"),
    );
    assert!(
        !status.success(),
        "the replay must reject a verdict that does not match the recorded allow: {status:?}"
    );
}

#[test]
fn the_recorded_allow_and_deny_verdicts_match_the_replay() {
    let binary = remuda_testing::ensure_workspace_bin("fake-claude-replay");
    assert!(
        drive_vcr_with_verdict(
            &binary,
            fixture("claude-exit-plan-mode-allow.jsonl"),
            Decision::Approve
        )
        .success(),
        "allow verdict must match the allow fixture"
    );
    assert!(
        drive_vcr_with_verdict(
            &binary,
            fixture("claude-exit-plan-mode-deny.jsonl"),
            Decision::Deny("plan review denied")
        )
        .success(),
        "deny verdict (exact message) must match the deny fixture"
    );
}

/// How the host test corrupts the recorded permission frame.
#[derive(Clone, Copy, Debug)]
enum Tamper {
    /// Keep behavior=allow but change the approved input.
    ChangedInput,
    /// Keep behavior=allow but attach an unauthorized permission escalation.
    AddedPermissions,
    /// Correct inner payload, but claim the envelope subtype is an error.
    WrongSubtype,
    /// Correct type/response, but add an unexpected ROOT field.
    ExtraRootField,
}

/// The whole-envelope verification must reject each corruption.
#[test]
fn the_replay_rejects_a_corrupted_allow_permission_envelope() {
    let binary = remuda_testing::ensure_workspace_bin("fake-claude-replay");
    for tamper in [
        Tamper::ChangedInput,
        Tamper::AddedPermissions,
        Tamper::WrongSubtype,
        Tamper::ExtraRootField,
    ] {
        let status = drive_vcr_tampered(
            &binary,
            fixture("claude-exit-plan-mode-allow.jsonl"),
            tamper,
        );
        assert!(
            !status.success(),
            "replay must reject tampered permission frame {tamper:?}: {status:?}"
        );
    }
}

fn drive_vcr_tampered(
    binary: &Path,
    fixture_path: PathBuf,
    tamper: Tamper,
) -> std::process::ExitStatus {
    let mut child = Command::new(binary)
        .env("FAKE_CLAUDE_FIXTURE", &fixture_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn replay");
    let raw = std::fs::read_to_string(&fixture_path).expect("read fixture");
    let frames: Vec<Value> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(serde_json::from_str::<Value>)
        .collect::<Result<_, _>>()
        .expect("fixture json");

    let mut stdin = child.stdin.take().expect("stdin");
    let init_id = format!("init-test-{}", std::process::id());
    for mut frame in frames {
        if frame.get("_dir").and_then(Value::as_str) != Some("in") {
            continue;
        }
        frame.as_object_mut().map(|o| o.remove("_dir"));
        if frame.pointer("/response/response/commands").is_some()
            && let Some(response) = frame
                .pointer_mut("/response")
                .and_then(Value::as_object_mut)
        {
            response.insert("request_id".into(), json!(init_id));
        }
        if frame.pointer("/response/response/behavior").is_some() {
            match tamper {
                Tamper::ChangedInput => {
                    frame
                        .pointer_mut("/response/response/updatedInput")
                        .expect("updatedInput present")
                        .as_object_mut()
                        .expect("input object")
                        .insert("plan".into(), json!("# a totally different plan"));
                }
                Tamper::AddedPermissions => {
                    frame
                        .pointer_mut("/response/response")
                        .and_then(Value::as_object_mut)
                        .expect("inner payload")
                        .insert(
                            "updatedPermissions".into(),
                            json!([{"type": "acceptEdits"}]),
                        );
                }
                Tamper::WrongSubtype => {
                    frame
                        .pointer_mut("/response")
                        .and_then(Value::as_object_mut)
                        .expect("response object")
                        .insert("subtype".into(), json!("error"));
                }
                Tamper::ExtraRootField => {
                    // type/response stay byte-identical; only an extra root
                    // key is added, which the complete-frame compare forbids.
                    frame
                        .as_object_mut()
                        .expect("permission frame is an object")
                        .insert("unexpected".into(), json!(true));
                }
            }
        }
        writeln!(stdin, "{frame}").expect("write checkpoint");
        stdin.flush().expect("flush");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("wait");
    if !output.status.success() {
        eprintln!("replay stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    // The assertion lives in the caller; the replay process reports the
    // rejection via its exit status (the corrupt frame is still written —
    // the peer, not this harness, is the verifier).
    output.status
}

/// Run the replay peer and drive every `_dir:"in"` checkpoint, rewriting the
/// init request id. At the permission checkpoint, send `decision` instead of
/// the recorded verdict. Returns the peer's exit status (non-zero on a
/// behavior/message mismatch).
fn drive_vcr_with_verdict(
    binary: &Path,
    fixture: PathBuf,
    decision: Decision,
) -> std::process::ExitStatus {
    let mut child = Command::new(binary)
        .env("FAKE_CLAUDE_FIXTURE", &fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn replay");
    let raw = std::fs::read_to_string(&fixture).expect("read fixture");
    let frames: Vec<Value> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .expect("fixture json");

    // Emit output is small (< pipe buffer); wait_with_output collects it.

    let mut stdin = child.stdin.take().expect("stdin");
    let init_id = format!("init-test-{}", std::process::id());
    for mut frame in frames {
        if frame.get("_dir").and_then(Value::as_str) != Some("in") {
            continue;
        }
        if let Some(obj) = frame.as_object_mut() {
            obj.remove("_dir");
        }
        if frame.pointer("/response/response/commands").is_some() {
            if let Some(response) = frame
                .pointer_mut("/response")
                .and_then(Value::as_object_mut)
            {
                response.insert("request_id".into(), json!(init_id));
            }
            writeln!(stdin, "{frame}").expect("write init");
            stdin.flush().expect("flush");
            continue;
        }
        if frame.pointer("/response/response/behavior").is_some() {
            // Recorded verdict the peer requires BEFORE we substitute ours.
            let expected_behavior = frame
                .pointer("/response/response/behavior")
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .to_string();
            let expected_message = frame
                .pointer("/response/response/message")
                .and_then(Value::as_str)
                .map(str::to_string);
            let response = frame
                .pointer_mut("/response/response")
                .and_then(Value::as_object_mut)
                .expect("permission response body");
            match decision {
                Decision::Approve => {
                    response.insert("behavior".into(), json!("allow"));
                    response.remove("message");
                }
                Decision::Deny(message) => {
                    response.insert("behavior".into(), json!("deny"));
                    response.insert("message".into(), json!(message));
                }
            }
            // The peer independently enforces this on its side; mirror the
            // check here to fail the test deterministically even if a future
            // peer change softened it.
            let got_behavior = response
                .get("behavior")
                .and_then(Value::as_str)
                .unwrap_or("<missing>");
            assert_ne!(
                (got_behavior, expected_behavior.as_str()),
                ("allow", "deny"),
                "test misconfigured: cannot send allow where a deny message is recorded"
            );
            let _ = expected_message;
        }
        writeln!(stdin, "{frame}").expect("write checkpoint");
        stdin.flush().expect("flush");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("wait");
    if !output.status.success() {
        eprintln!("replay stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    output.status
}
