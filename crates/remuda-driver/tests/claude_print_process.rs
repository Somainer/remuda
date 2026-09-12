//! Process tests for `ClaudePrintDriver` against `fake-claude` scripts.

use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use remuda_driver::{
    BinaryPin, BinarySource, Driver, DriverError, ProviderHealth, ProviderKind, ProviderProfile,
    SecretRef,
};
use remuda_protocol::{
    ApprovalAnswer, ClaudeInteractionMode, ClaudePermission, ClaudePermissionMode, ContentBlock,
    Digest, DispatchState, DriverInput, InputOrigin, InstanceSpec, InteractionAnswer,
    InteractionKind, Knowledge, LifecyclePayload, Observation, ObservationPayload, PermissionMode,
    PromptInput, PromptMode, QuestionAnswer, QuestionFieldAnswer, TextBlock, WorkflowState,
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

fn load_spec() -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.cwd = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    spec
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: "https://gateway.example".into(),
        delegation: remuda_driver::Delegation::None,
        secret_ref: Some(SecretRef::parse("env:REMUDA_CLAUDE_PRINT_SECRET").unwrap()),
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn driver_for(
    kind: ScriptKind,
    spec_mode: Option<ClaudePermissionMode>,
) -> (ClaudePrintDriver, InstanceSpec) {
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
        ClaudePrintOptions::new(profile(), launch, home, BinarySource::Pinned(pin_fake()));
    options.extra_env = extra;
    options.handshake_timeout = Duration::from_secs(5);
    let mut spec = load_spec();
    spec.cwd = tmp
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    if let Some(mode) = spec_mode {
        spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
            mode,
            interaction: ClaudeInteractionMode::Host,
        }));
    }
    // Keep tempdir alive by leaking; tests are short-lived.
    std::mem::forget(tmp);
    (ClaudePrintDriver::new(options), spec)
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
            Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}

fn kinds(obs: &[Observation]) -> Vec<&'static str> {
    obs.iter()
        .map(|o| match &o.body {
            ObservationPayload::Lifecycle(_) => "lifecycle",
            ObservationPayload::Message(_) => "message",
            ObservationPayload::Thought(_) => "thought",
            ObservationPayload::ToolCall(_) => "tool_call",
            ObservationPayload::ToolResult(_) => "tool_result",
            ObservationPayload::InteractionRequested(_) => "interaction.requested",
            ObservationPayload::Usage(_) => "usage",
            ObservationPayload::WorkflowRun(_) => "workflow.run",
            ObservationPayload::WorkflowPhase(_) => "workflow.phase",
            ObservationPayload::Opaque(_) => "opaque",
            other => other.kind().as_str(),
        })
        .collect()
}

trait KindName {
    fn as_str(&self) -> &'static str;
}

impl KindName for remuda_protocol::ObservationKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Thought => "thought",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::InteractionRequested => "interaction.requested",
            Self::InteractionAnswered => "interaction.answered",
            Self::InteractionExpired => "interaction.expired",
            Self::WorkflowRun => "workflow.run",
            Self::WorkflowPhase => "workflow.phase",
            Self::WorkflowMember => "workflow.member",
            Self::Lifecycle => "lifecycle",
            Self::Usage => "usage",
            Self::Artifact => "artifact",
            Self::RawTty => "raw_tty",
            Self::Opaque => "opaque",
        }
    }
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

fn turn_done_count(obs: &[Observation]) -> usize {
    obs.iter()
        .filter(|o| lifecycle_status(o) == Some("turn_done"))
        .count()
}

#[tokio::test]
async fn ok_script_emits_init_message_and_turn_done() {
    let (driver, spec) = driver_for(ScriptKind::Ok, None);
    let mut handle = driver.start(spec).await.expect("start");
    assert_eq!(handle.ack().dispatch, DispatchState::TransportWritten);
    let ack = driver.send(prompt("hi")).await.expect("send");
    assert_eq!(ack.dispatch, DispatchState::TransportWritten);
    let events = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 1
    })
    .await;
    let names = kinds(&events);
    assert!(
        names.contains(&"lifecycle"),
        "expected init lifecycle, got {names:?}"
    );
    assert!(
        names.contains(&"message"),
        "expected assistant message, got {names:?}"
    );
    assert!(
        turn_done_count(&events) >= 1,
        "expected turn_done, got {names:?}"
    );
    assert!(
        names.contains(&"usage")
            || events
                .iter()
                .any(|o| lifecycle_status(o) == Some("started"))
    );
    driver.close().await.expect("close");
}

#[tokio::test]
async fn approval_allow_and_deny() {
    let (driver, spec) = driver_for(ScriptKind::Approval, None);
    let mut handle = driver.start(spec).await.expect("start");
    driver.send(prompt("touch")).await.expect("send");
    let events = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        obs.iter()
            .any(|o| matches!(o.body, ObservationPayload::InteractionRequested(_)))
    })
    .await;
    let requested = events
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::InteractionRequested(payload) => Some(payload.interaction.clone()),
            _ => None,
        })
        .expect("interaction");
    assert_eq!(requested.kind, InteractionKind::Approval);
    let ack = driver
        .respond_interaction(
            requested.meta.id.clone(),
            InteractionAnswer::Approval(Box::new(ApprovalAnswer {
                option_id: "allow".into(),
                input_digest: requested.request.clone().pipe_digest(),
            })),
        )
        .await
        .expect("allow");
    assert_eq!(ack.dispatch, DispatchState::TransportWritten);
    let rest = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        obs.iter()
            .any(|o| matches!(o.body, ObservationPayload::ToolResult(_)))
            && turn_done_count(obs) >= 1
    })
    .await;
    assert!(
        rest.iter()
            .any(|o| matches!(o.body, ObservationPayload::ToolResult(_))),
        "allow path should emit tool_result"
    );
    driver.close().await.expect("close");

    let (driver, spec) = driver_for(ScriptKind::Approval, None);
    let mut handle = driver.start(spec).await.expect("start deny");
    driver.send(prompt("touch")).await.expect("send");
    let events = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        obs.iter()
            .any(|o| matches!(o.body, ObservationPayload::InteractionRequested(_)))
    })
    .await;
    let requested = events
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::InteractionRequested(payload) => Some(payload.interaction.clone()),
            _ => None,
        })
        .expect("interaction deny");
    driver
        .respond_interaction(
            requested.meta.id.clone(),
            InteractionAnswer::Approval(Box::new(ApprovalAnswer {
                option_id: "deny".into(),
                input_digest: dummy_digest(),
            })),
        )
        .await
        .expect("deny");
    let rest = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 1
    })
    .await;
    assert!(
        rest.iter()
            .any(|o| matches!(o.body, ObservationPayload::ToolResult(_)))
            || turn_done_count(&rest) >= 1
    );
    driver.close().await.expect("close");
}

#[tokio::test]
async fn askuser_answers_question() {
    let (driver, spec) = driver_for(ScriptKind::AskUser, None);
    let mut handle = driver.start(spec).await.expect("start");
    driver.send(prompt("ask")).await.expect("send");
    let events = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        obs.iter()
            .any(|o| matches!(o.body, ObservationPayload::InteractionRequested(_)))
    })
    .await;
    let requested = events
        .iter()
        .find_map(|o| match &o.body {
            ObservationPayload::InteractionRequested(payload) => Some(payload.interaction.clone()),
            _ => None,
        })
        .expect("question");
    assert_eq!(requested.kind, InteractionKind::Question);
    let mut answers = BTreeMap::new();
    answers.insert(
        "q0".into(),
        QuestionFieldAnswer {
            option_ids: vec!["Tea".into()],
            text: Some("Tea".into()),
        },
    );
    driver
        .respond_interaction(
            requested.meta.id.clone(),
            InteractionAnswer::Question(Box::new(QuestionAnswer { answers })),
        )
        .await
        .expect("answer");
    let rest = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 1
    })
    .await;
    assert!(turn_done_count(&rest) >= 1);
    driver.close().await.expect("close");
}

#[tokio::test]
async fn workflow_emits_two_results() {
    let (driver, spec) = driver_for(ScriptKind::Workflow, None);
    let mut handle = driver.start(spec).await.expect("start");
    driver.send(prompt("wf")).await.expect("send");
    let events = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        turn_done_count(obs) >= 2
    })
    .await;
    assert!(
        turn_done_count(&events) >= 2,
        "Workflow emits two results, got {:?}",
        kinds(&events)
    );
    assert!(
        events
            .iter()
            .any(|o| matches!(o.body, ObservationPayload::WorkflowRun(_))),
        "expected workflow.run"
    );
    let completed = events.iter().any(|o| match &o.body {
        ObservationPayload::WorkflowRun(run) => {
            matches!(run.state, WorkflowState::Completed)
        }
        _ => false,
    });
    assert!(
        completed
            || events
                .iter()
                .any(|o| matches!(o.body, ObservationPayload::WorkflowPhase(_)))
    );
    driver.close().await.expect("close");
}

#[tokio::test]
async fn kill_emits_session_exit_lifecycle() {
    let (driver, spec) = driver_for(ScriptKind::Ok, None);
    let mut handle = driver.start(spec).await.expect("start");
    driver.kill().await.expect("kill");
    let events = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        obs.iter().any(|o| lifecycle_status(o) == Some("exited"))
    })
    .await;
    assert!(
        events.iter().any(|o| lifecycle_status(o) == Some("exited")),
        "expected exited lifecycle, got {:?}",
        kinds(&events)
    );
}

#[tokio::test]
async fn bot_bypass_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let mut options = ClaudePrintOptions::new(
        profile(),
        tmp.path().join("launch"),
        tmp.path().join("home"),
        BinarySource::Pinned(pin_fake()),
    );
    options.origin = InputOrigin::Bot;
    std::fs::create_dir_all(&options.launch_dir).unwrap();
    std::fs::create_dir_all(&options.native_home).unwrap();
    let driver = ClaudePrintDriver::new(options);
    let mut spec = load_spec();
    spec.cwd = tmp
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: ClaudeInteractionMode::Host,
    }));
    let error = driver.start(spec).await.expect_err("bypass");
    assert!(matches!(error, DriverError::BypassNotAllowedForBot));
}

#[tokio::test]
#[ignore = "requires local `claude` and spends haiku budget"]
async fn live_haiku_ok() {
    let cwd = PathBuf::from("/tmp/remuda-driver");
    std::fs::create_dir_all(&cwd).unwrap();
    let options = ClaudePrintOptions::new(
        profile(),
        cwd.join("launch"),
        cwd.join("home"),
        BinarySource::Command("claude".into()),
    );
    std::fs::create_dir_all(&options.launch_dir).unwrap();
    std::fs::create_dir_all(&options.native_home).unwrap();
    let driver = ClaudePrintDriver::new(options);
    let mut spec = load_spec();
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("haiku".into());
    spec.args = vec!["--max-budget-usd".into(), "0.3".into()];
    let mut handle = driver.start(spec).await.expect("live start");
    driver
        .send(prompt("Reply with exactly OK. Do not use tools."))
        .await
        .expect("live send");
    let events = collect_until(&mut handle, Duration::from_secs(90), |obs| {
        turn_done_count(obs) >= 1
    })
    .await;
    assert!(turn_done_count(&events) >= 1);
    driver.close().await.expect("close");
}

trait PipeDigest {
    fn pipe_digest(self) -> Digest;
}

impl PipeDigest for remuda_protocol::InteractionRequest {
    fn pipe_digest(self) -> Digest {
        match self {
            remuda_protocol::InteractionRequest::Approval(req) => req.input_digest,
            _ => dummy_digest(),
        }
    }
}
