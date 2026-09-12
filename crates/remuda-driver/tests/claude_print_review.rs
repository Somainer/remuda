//! Review tests for `claude_print.rs` (handshake, control_response, results, resume).
//! Does not share helpers with `claude_print_process.rs`.

use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions, review};
use remuda_driver::{
    BinaryPin, BinarySource, Driver, DriverError, ProviderHealth, ProviderKind, ProviderProfile,
    SecretRef,
};
use remuda_protocol::{
    AgentKind, ApprovalAnswer, ClaudePermission, ClaudePermissionMode, ClaudeRef, ContentBlock,
    Digest, DriverInput, InputOrigin, InstanceSpec, InteractionAnswer, Knowledge, LifecyclePayload,
    NativeRef, Observation, ObservationPayload, PermissionMode, PromptInput, PromptMode, TextBlock,
};
use remuda_testing::{ScriptKind, ensure_workspace_bin, script_path};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

fn digest() -> Digest {
    Digest::try_from(format!("sha256:{:0>64}", "b")).unwrap()
}

fn ensure_fake_claude() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let path = ensure_workspace_bin("fake-claude");
        assert!(path.is_file(), "missing {}", path.display());
        path
    })
    .clone()
}

fn pin() -> BinaryPin {
    let path = ensure_fake_claude().canonicalize().expect("fake-claude");
    BinaryPin {
        abs_path: path.to_string_lossy().into_owned(),
        version: "fake-claude".into(),
        sha256: digest(),
    }
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: "https://gateway.example".into(),
        delegation: remuda_driver::Delegation::Gateway,
        secret_ref: Some(SecretRef::parse("env:REMUDA_CLAUDE_PRINT_SECRET").unwrap()),
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn load_spec() -> InstanceSpec {
    serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap()
}

fn driver_for(
    kind: ScriptKind,
    spec: InstanceSpec,
) -> (tempfile::TempDir, ClaudePrintDriver, InstanceSpec, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let cwd = tmp.path().canonicalize().unwrap();
    let mut extra = BTreeMap::new();
    extra.insert(
        "FAKE_CLAUDE_SCRIPT".into(),
        script_path(kind).to_string_lossy().into_owned(),
    );
    extra.insert("ANTHROPIC_AUTH_TOKEN".into(), "sk-review".into());
    let mut options = ClaudePrintOptions::new(profile(), launch, home, BinarySource::Pinned(pin()));
    options.extra_env = extra;
    options.handshake_timeout = Duration::from_secs(5);
    let mut spec = spec;
    spec.cwd = cwd.to_string_lossy().into_owned();
    (tmp, ClaudePrintDriver::new(options), spec, cwd)
}

fn prompt(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "msg-review".into(),
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

fn native_affects(obs: &Observation) -> Option<(String, bool)> {
    match &obs.body {
        ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
            LifecyclePayload::Native(native) => {
                Some((native.native_name.clone(), native.affects_completion))
            }
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn permission_allow_is_camel_case_and_echoes_request_id() {
    let input = json!({"command": "true"});
    let answer = InteractionAnswer::Approval(Box::new(ApprovalAnswer {
        option_id: "allow".into(),
        input_digest: digest(),
    }));
    let value = review::permission_control_json("cli-req-1", &input, &answer).unwrap();
    assert_eq!(value["type"], "control_response");
    assert_eq!(value["response"]["subtype"], "success");
    assert_eq!(value["response"]["request_id"], "cli-req-1");
    assert_eq!(value["response"]["response"]["behavior"], "allow");
    assert_eq!(
        value["response"]["response"]["updatedInput"]["command"],
        "true"
    );
    let encoded = value.to_string();
    assert!(encoded.contains("updatedInput"));
    assert!(!encoded.contains("updated_input"));
}

#[test]
fn unknown_stdout_type_is_opaque_not_fatal() {
    let obs = review::map_stdout_json(json!({"type": "not-a-real-frame", "x": 1})).unwrap();
    assert_eq!(obs.len(), 1);
    assert!(matches!(obs[0].body, ObservationPayload::Opaque(_)));
}

#[test]
fn first_workflow_result_does_not_complete_the_run() {
    let first = review::map_stdout_json(json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result_index": 0,
        "queued_turn_count": 0,
        "result": "waiting"
    }))
    .unwrap();
    let second = review::map_stdout_json(json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result_index": 1,
        "queued_turn_count": 0,
        "result": "OK"
    }))
    .unwrap();
    let a = first
        .iter()
        .find_map(native_affects)
        .expect("first result lifecycle");
    let b = second
        .iter()
        .find_map(native_affects)
        .expect("second result lifecycle");
    assert_eq!(a.0, "result");
    assert!(!a.1, "result_index 0 must not affect completion");
    assert_eq!(b.0, "result");
    assert!(b.1, "later result_index may affect completion");
}

#[test]
fn bot_origin_rejects_bypass_and_dont_ask() {
    let mut spec = load_spec();
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: remuda_protocol::ClaudeInteractionMode::Host,
    }));
    assert!(matches!(
        review::reject_bot(&spec, InputOrigin::Bot),
        Err(DriverError::BypassNotAllowedForBot)
    ));
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::DontAsk,
        interaction: remuda_protocol::ClaudeInteractionMode::Host,
    }));
    assert!(matches!(
        review::reject_bot(&spec, InputOrigin::Agent),
        Err(DriverError::BypassNotAllowedForBot)
    ));
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::Manual,
        interaction: remuda_protocol::ClaudeInteractionMode::Host,
    }));
    assert!(review::reject_bot(&spec, InputOrigin::Bot).is_ok());
}

#[test]
fn refuse_bare_and_no_session_persistence() {
    assert!(review::refuse_argv(&["-p".into(), "--verbose".into()]).is_ok());
    assert!(review::refuse_argv(&["--bare".into()]).is_err());
    assert!(review::refuse_argv(&["--no-session-persistence".into()]).is_err());
}

#[tokio::test]
async fn start_completes_initialize_before_user_and_keeps_stdin_open() {
    let spec = load_spec();
    let (_tmp, driver, spec, _) = driver_for(ScriptKind::Ok, spec);
    let mut handle = driver.start(spec).await.expect("handshake must complete");
    assert!(
        !handle
            .recipe()
            .argv
            .iter()
            .any(|t| t == "--bare" || t == "--no-session-persistence")
    );
    driver.send(prompt("one")).await.expect("first user");
    let first = collect_until(&mut handle, Duration::from_secs(5), |obs| {
        obs.iter()
            .any(|o| native_affects(o).map(|p| p.0) == Some("result".into()))
    })
    .await;
    assert!(
        first
            .iter()
            .any(|o| native_affects(o).is_some_and(|(name, _)| name == "session")),
        "system/init should map after initialize"
    );
    driver.send(prompt("two")).await.expect("stdin still open");
    driver.close().await.expect("close");
}

#[tokio::test]
async fn workflow_keeps_reading_after_first_result() {
    let spec = load_spec();
    let (_tmp, driver, spec, _) = driver_for(ScriptKind::Workflow, spec);
    let mut handle = driver.start(spec).await.expect("start");
    driver.send(prompt("wf")).await.expect("send");
    let events = collect_until(&mut handle, Duration::from_secs(8), |obs| {
        obs.iter()
            .filter(|o| native_affects(o).is_some_and(|(name, _)| name == "result"))
            .count()
            >= 2
    })
    .await;
    let results: Vec<_> = events.iter().filter_map(native_affects).collect();
    let result_frames: Vec<_> = results
        .into_iter()
        .filter(|(name, _)| name == "result")
        .collect();
    assert!(
        result_frames.len() >= 2,
        "expected two result frames, got {result_frames:?}"
    );
    assert!(
        !result_frames[0].1,
        "first workflow result must not complete the run"
    );
    driver.close().await.expect("close");
}

#[tokio::test]
async fn resume_reapplies_settings_and_model() {
    let mut spec = load_spec();
    spec.model_id = Some("haiku".into());
    let (_tmp, driver, spec, _) = driver_for(ScriptKind::Ok, spec.clone());
    let handle = driver.start(spec.clone()).await.expect("start");
    let first_argv = handle.recipe().argv.clone();
    assert!(
        first_argv
            .windows(2)
            .any(|w| w[0] == "--model" && w[1] == "haiku")
    );
    assert!(first_argv.windows(2).any(|w| w[0] == "--setting-sources"));
    assert!(first_argv.windows(2).any(|w| w[0] == "--settings"));
    let session = handle
        .ack()
        .native_ids
        .get("sessionId")
        .cloned()
        .expect("session");
    driver.close().await.expect("close");

    let native = NativeRef {
        host_id: spec.host.clone(),
        native_store_id: spec.binary_ref.clone(),
        kind: AgentKind::Claude,
        session_id: Knowledge::Known {
            value: session.clone(),
        },
        transcript: Knowledge::Unknown {
            reason: "test".into(),
            evidence_event_ids: vec![],
        },
        codex: None,
        acp: None,
        claude: Some(ClaudeRef {
            session_id: session.clone(),
        }),
        claude_bg: None,
        agy: None,
        herdr: None,
    };
    let resumed = driver.resume(native).await.expect("resume");
    let argv = &resumed.recipe().argv;
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--resume" && w[1] == session)
    );
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--model" && w[1] == "haiku")
    );
    assert!(argv.windows(2).any(|w| w[0] == "--settings"));
    assert!(argv.windows(2).any(|w| w[0] == "--setting-sources"));
    assert!(
        !argv
            .iter()
            .any(|t| t == "--bare" || t == "--no-session-persistence")
    );
    driver.close().await.expect("close resume");
}

#[tokio::test]
async fn bot_dont_ask_is_rejected_at_start() {
    let mut spec = load_spec();
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::DontAsk,
        interaction: remuda_protocol::ClaudeInteractionMode::Host,
    }));
    let tmp = tempfile::tempdir().unwrap();
    let dummy = BinaryPin {
        abs_path: "/nonexistent-claude-review".into(),
        version: "review".into(),
        sha256: digest(),
    };
    let mut options = ClaudePrintOptions::new(
        profile(),
        tmp.path().join("l"),
        tmp.path().join("h"),
        BinarySource::Pinned(dummy),
    );
    options.origin = InputOrigin::Bot;
    std::fs::create_dir_all(&options.launch_dir).unwrap();
    std::fs::create_dir_all(&options.native_home).unwrap();
    spec.cwd = tmp
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let driver = ClaudePrintDriver::new(options);
    let err = driver.start(spec).await.expect_err("bot dontAsk");
    assert!(matches!(err, DriverError::BypassNotAllowedForBot));
}
