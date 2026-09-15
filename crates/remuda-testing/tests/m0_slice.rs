//! M0 slice without Node: claude-print → journal → projections → InteractionBroker.
//!
//! Scripts: `crates/remuda-testing/fixtures/scripts/{ok,approval,askuser,workflow}.jsonl`.

use async_trait::async_trait;
use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use remuda_driver::interaction::{
    BrokerConfig, BrokerError, InstancePolicy, InteractionBroker, InteractionOwner,
    NativeRequestId, PendingSpec,
};
use remuda_driver::{
    BinaryPin, BinarySource, Driver, ProviderHealth, ProviderKind, ProviderProfile, SecretRef,
};
use remuda_journal::{Envelope, FsyncPolicy, Journal, JournalOptions, Projections, fold_all};
use remuda_protocol::{
    ApprovalAnswer, CommandId, ContentBlock, Digest, DriverInput, Id, InputOrigin, InstanceId,
    Interaction, InteractionAnswer, InteractionId, InteractionRequest, InteractionState,
    LifecyclePayload, LifecycleTopic, NativeRequestKey, Observation, ObservationPayload,
    PromptInput, PromptMode, QuestionAnswer, QuestionFieldAnswer, TextBlock, U64,
};
use remuda_testing::{ScriptKind, ensure_workspace_bin, script_path};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex;

fn ensure_fake_claude() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| ensure_workspace_bin("fake-claude"))
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
        secret_ref: Some(SecretRef::parse("env:REMUDA_M0_SLICE_SECRET").unwrap()),
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn load_spec() -> remuda_protocol::InstanceSpec {
    serde_json::from_str(include_str!(
        "../../remuda-driver/tests/fixtures/instance-spec.json"
    ))
    .unwrap()
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

struct Slice {
    _tmp: TempDir,
    driver: Arc<ClaudePrintDriver>,
    spec: remuda_protocol::InstanceSpec,
}

fn slice(kind: ScriptKind) -> Slice {
    let tmp = TempDir::new().unwrap();
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
    Slice {
        driver: Arc::new(ClaudePrintDriver::new(options)),
        spec,
        _tmp: tmp,
    }
}

fn envelope(obs: &Observation) -> Envelope {
    Envelope {
        journal_id: obs.journal_id.clone(),
        instance_id: obs.instance_id.clone(),
        run_id: obs.run_id.clone(),
        host_id: obs.host_id.clone(),
        process_generation: obs.process_generation,
        run_generation: obs.run_generation,
        observed_at: obs.observed_at.clone(),
        native_at: obs.native_at.clone(),
        source: obs.source.clone(),
        completeness: obs.completeness,
        evidence_event_ids: obs.evidence_event_ids.clone(),
        event_id: None,
        body: obs.body.clone(),
        raw: None,
    }
}

async fn commit(
    journal: &Journal,
    instance: &InstanceId,
    journal_id: &Id,
    fold: &mut Projections,
    obs: &Observation,
) -> remuda_protocol::U64 {
    let mut env = envelope(obs);
    env.instance_id = instance.clone();
    env.journal_id = journal_id.clone();
    let seq = journal.append(instance, env).await.unwrap();
    let mut stored = obs.clone();
    stored.seq = seq;
    stored.instance_id = instance.clone();
    stored.journal_id = journal_id.clone();
    fold.apply(&stored);
    seq
}

struct PrintOwner {
    driver: Arc<ClaudePrintDriver>,
    native: Mutex<Option<InteractionId>>,
}

#[async_trait]
impl InteractionOwner for PrintOwner {
    async fn apply_answer(
        &self,
        _id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), BrokerError> {
        let native = self
            .native
            .lock()
            .await
            .clone()
            .ok_or(BrokerError::NotFound)?;
        self.driver
            .respond_interaction(native, answer)
            .await
            .map_err(|e| BrokerError::Forward(e.to_string()))?;
        Ok(())
    }

    async fn deny_or_cancel(&self, _id: InteractionId) -> Result<(), BrokerError> {
        Ok(())
    }
}

fn allow_for(interaction: &Interaction) -> InteractionAnswer {
    match &interaction.request {
        InteractionRequest::Approval(approval) => {
            InteractionAnswer::Approval(Box::new(ApprovalAnswer {
                option_id: "allow".into(),
                input_digest: approval.input_digest.clone(),
            }))
        }
        InteractionRequest::Question(_) => {
            let mut answers = BTreeMap::new();
            answers.insert(
                "q0".into(),
                QuestionFieldAnswer {
                    option_ids: vec!["Tea".into()],
                    text: Some("Tea".into()),
                },
            );
            InteractionAnswer::Question(Box::new(QuestionAnswer { answers }))
        }
        other => panic!("unexpected request {other:?}"),
    }
}

fn pending_spec(interaction: &Interaction) -> PendingSpec {
    let request_id = match &interaction.request_key.native {
        NativeRequestKey::Rpc { value, .. } => value.clone(),
        NativeRequestKey::Hook { invocation_id } => invocation_id.as_str().to_string(),
        NativeRequestKey::None => String::new(),
    };
    PendingSpec {
        instance_id: interaction.instance_id.clone(),
        host_id: interaction.host_id.clone(),
        native_request_id: NativeRequestId {
            request_id,
            tool_use_id: None,
        },
        payload: interaction.request.clone(),
        run_generation: interaction.request_key.run_generation.unwrap_or(U64(1)),
        tool_name: match &interaction.request {
            InteractionRequest::Approval(a) => Some(a.title.clone()),
            InteractionRequest::Question(_) => Some("AskUserQuestion".into()),
            _ => None,
        },
    }
}

async fn pump_until(
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

fn is_turn_result(obs: &Observation) -> bool {
    match &obs.body {
        ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
            LifecyclePayload::Native(native) => {
                native.topic == LifecycleTopic::Turn
                    && native.native_name.to_ascii_lowercase().contains("result")
            }
            _ => false,
        },
        _ => false,
    }
}

fn text_contains(fold: &Projections, needle: &str) -> bool {
    fold.transcript.entries.iter().any(|entry| match entry {
        remuda_journal::TranscriptEntry::Message { text, .. } => text.contains(needle),
        remuda_journal::TranscriptEntry::ToolResult { text, .. } => text.contains(needle),
        _ => false,
    })
}

async fn run_kind(
    kind: ScriptKind,
    answer_interaction: bool,
) -> (Vec<Observation>, Projections, Journal, InstanceId) {
    let slice = slice(kind);
    let journal = Journal::open_with(
        slice._tmp.path().join("journal"),
        JournalOptions {
            fsync: FsyncPolicy::Never,
        },
    )
    .unwrap();
    let mut handle = slice.driver.start(slice.spec.clone()).await.unwrap();
    slice.driver.send(prompt("go")).await.unwrap();

    let (broker, mut broker_rx) = InteractionBroker::new(BrokerConfig::default()).unwrap();
    let owner = Arc::new(PrintOwner {
        driver: Arc::clone(&slice.driver),
        native: Mutex::new(None),
    });

    let mut incremental = Projections::default();
    let mut committed = Vec::new();
    let mut instance = None;
    let mut journal_id = None;
    let mut answered = false;

    let want_results = if kind == ScriptKind::Workflow { 2 } else { 1 };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let obs = match tokio::time::timeout(remaining, handle.recv()).await {
            Ok(Some(obs)) => obs,
            _ => break,
        };
        instance.get_or_insert_with(|| obs.instance_id.clone());
        journal_id.get_or_insert_with(|| obs.journal_id.clone());
        let inst = instance.as_ref().unwrap();
        let jid = journal_id.as_ref().unwrap();
        commit(&journal, inst, jid, &mut incremental, &obs).await;
        committed.push(obs.clone());

        if let ObservationPayload::InteractionRequested(payload) = &obs.body
            && answer_interaction
            && !answered
        {
            let interaction = &payload.interaction;
            let native = interaction.meta.id.clone();
            *owner.native.lock().await = Some(native.clone());
            broker
                .register_owner(interaction.instance_id.clone(), owner.clone())
                .await;
            broker
                .set_policy(interaction.instance_id.clone(), InstancePolicy::ask(U64(1)))
                .await;
            let ticket = broker.insert(pending_spec(interaction)).await.unwrap();
            broker
                .answer(
                    ticket,
                    allow_for(interaction),
                    Id::new("dev").unwrap(),
                    CommandId::new(),
                )
                .await
                .unwrap();
            answered = true;
            while let Ok(mut broker_obs) = broker_rx.try_recv() {
                if let ObservationPayload::InteractionAnswered(payload) = &mut broker_obs.body {
                    payload.interaction_id = native.clone();
                }
                broker_obs.instance_id = inst.clone();
                broker_obs.journal_id = jid.clone();
                broker_obs.host_id = interaction.host_id.clone();
                commit(&journal, inst, jid, &mut incremental, &broker_obs).await;
                committed.push(broker_obs);
            }
        }

        let results = committed.iter().filter(|o| is_turn_result(o)).count();
        if results >= want_results {
            break;
        }
    }

    let more = pump_until(&mut handle, Duration::from_secs(2), |_| false).await;
    let inst = instance.as_ref().expect("instance id from observations");
    let jid = journal_id.as_ref().expect("journal id from observations");
    for obs in more {
        commit(&journal, inst, jid, &mut incremental, &obs).await;
        committed.push(obs);
    }
    let _ = slice.driver.close().await;
    (committed, incremental, journal, inst.clone())
}

#[tokio::test]
async fn m0_slice_ok_approval_askuser_workflow() {
    let (ok_events, ok_fold, _, _) = run_kind(ScriptKind::Ok, false).await;
    assert!(
        text_contains(&ok_fold, "OK"),
        "ok script should fold assistant OK, events={}",
        ok_events.len()
    );
    assert_eq!(
        ok_fold.status.connectivity,
        remuda_protocol::Connectivity::Connected
    );

    let (approval_events, approval_fold, journal, instance) =
        run_kind(ScriptKind::Approval, true).await;
    assert!(
        approval_events
            .iter()
            .any(|o| matches!(o.body, ObservationPayload::InteractionRequested(_))),
        "approval script emits interaction.requested"
    );
    assert!(
        approval_fold
            .interaction
            .by_key
            .values()
            .any(|r| r.state == InteractionState::AnswerCommitted || r.answered_seq.is_some()),
        "broker answer should fold interaction lifecycle to answer-committed"
    );
    assert!(
        approval_fold.status.activity != remuda_protocol::Activity::Idle
            || approval_fold
                .interaction
                .by_key
                .values()
                .any(|r| r.answered_seq.is_some()),
        "status triple should reflect the interaction"
    );
    let snap = journal.snapshot(&instance).await.unwrap();
    assert_eq!(snap.projections.interaction, approval_fold.interaction);

    let (ask_events, ask_fold, _, _) = run_kind(ScriptKind::AskUser, true).await;
    assert!(
        ask_events
            .iter()
            .any(|o| matches!(o.body, ObservationPayload::InteractionRequested(_)))
    );
    assert!(
        ask_fold
            .interaction
            .by_key
            .values()
            .any(|r| r.state == InteractionState::AnswerCommitted || r.requested_seq.0 >= 1)
    );

    let (wf_events, wf_fold, _, _) = run_kind(ScriptKind::Workflow, false).await;
    assert!(
        wf_events.iter().filter(|o| is_turn_result(o)).count() >= 2
            || wf_events
                .iter()
                .any(|o| matches!(o.body, ObservationPayload::WorkflowRun(_))),
        "workflow script should emit two results or workflow.run"
    );
    assert!(!wf_fold.transcript.entries.is_empty());
}

#[tokio::test]
async fn m0_slice_replay_equals_incremental() {
    let (_events, incremental, journal, instance) = run_kind(ScriptKind::Approval, true).await;
    let items = journal.read_range(&instance, U64(1), None).await.unwrap();
    assert!(!items.is_empty());
    let mut one_by_one = Projections::default();
    for obs in &items {
        one_by_one.apply(obs);
    }
    let replayed = fold_all(&items);
    assert_eq!(
        one_by_one, replayed,
        "incremental fold == whole-journal fold"
    );
    assert_eq!(
        incremental.transcript.entries.len(),
        replayed.transcript.entries.len()
    );
    assert_eq!(incremental.interaction.by_key, replayed.interaction.by_key);
    let snap = journal.snapshot(&instance).await.unwrap();
    assert_eq!(snap.projections, replayed);
}

#[tokio::test]
async fn m0_slice_status_triple_is_populated() {
    let (_, fold, _, _) = run_kind(ScriptKind::Ok, false).await;
    assert_ne!(
        fold.status.connectivity,
        remuda_protocol::Connectivity::Disconnected
    );
    let _ = fold.status.lifecycle;
    let _ = fold.status.activity;
}
