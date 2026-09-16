//! Drive one bundled fake-claude script through `ClaudePrintDriver` + `remuda-journal`.

use anyhow::{Context, Result, bail};
use clap::Parser;
use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use remuda_driver::{
    BinaryPin, BinarySource, Driver, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_journal::{Envelope, FsyncPolicy, Journal, JournalOptions, fold_all};
use remuda_protocol::{
    ApprovalAnswer, ContentBlock, Digest, DriverInput, InputOrigin, InstanceSpec,
    InteractionAnswer, InteractionKind, InteractionRequest, Knowledge, LifecyclePayload,
    Observation, ObservationPayload, PromptInput, PromptMode, QuestionAnswer, QuestionFieldAnswer,
    TextBlock, U64, WorkflowState,
};
use remuda_testing::{ScriptKind, script_kind_from_name, script_path};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const INSTANCE_SPEC: &str =
    include_str!("../../../remuda-driver/tests/fixtures/instance-spec.json");

#[derive(Parser, Debug)]
#[command(about = "M0 stub: claude-print driver + journal for one fake-claude script")]
struct Args {
    /// Bundled script name: ok, approval, askuser, workflow.
    #[arg(long)]
    script: String,
    /// Absolute path to the fake-claude binary.
    #[arg(long)]
    fake_claude: PathBuf,
    /// Working directory (launch overlay, native home, journal).
    #[arg(long)]
    workdir: PathBuf,
    /// Per-script event wait, milliseconds.
    #[arg(long, default_value_t = 12_000)]
    timeout_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("remuda_driver=warn".parse()?),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let args = Args::parse();
    let kind = script_kind_from_name(&args.script)
        .with_context(|| format!("unknown script {}", args.script))?;
    fs::create_dir_all(&args.workdir)?;
    let summary = run_script(kind, &args).await?;
    let path = args.workdir.join("summary.json");
    fs::write(&path, serde_json::to_vec_pretty(&summary)?)?;
    println!("{}", serde_json::to_string(&summary)?);
    if summary.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!(
            "script {} failed: {}",
            args.script,
            summary
                .get("failure")
                .and_then(Value::as_str)
                .unwrap_or("assertions")
        );
    }
    Ok(())
}

async fn run_script(kind: ScriptKind, args: &Args) -> Result<Value> {
    let launch = args.workdir.join("launch");
    let home = args.workdir.join("home");
    let journal_dir = args.workdir.join("journal-data");
    fs::create_dir_all(&launch)?;
    fs::create_dir_all(&home)?;
    fs::create_dir_all(args.workdir.join("transcripts"))?;

    let pin = pin_fake(&args.fake_claude)?;
    let mut extra = BTreeMap::new();
    extra.insert(
        "FAKE_CLAUDE_SCRIPT".into(),
        script_path(kind).to_string_lossy().into_owned(),
    );
    extra.insert(
        "FAKE_CLAUDE_TRANSCRIPT_DIR".into(),
        args.workdir
            .join("transcripts")
            .to_string_lossy()
            .into_owned(),
    );
    let mut options = ClaudePrintOptions::new(profile(), launch, home, BinarySource::Pinned(pin));
    options.extra_env = extra;
    options.handshake_timeout = Duration::from_secs(8);

    let spec = load_spec(&args.workdir)?;

    let driver = ClaudePrintDriver::new(options);
    let mut handle = driver.start(spec).await.context("start")?;
    driver.send(prompt("m0-stub")).await.context("send")?;

    let timeout = Duration::from_millis(args.timeout_ms);
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, handle.recv()).await {
            Ok(Some(obs)) => {
                maybe_answer(&driver, kind, &obs).await?;
                events.push(obs);
                if done(kind, &events) {
                    drain(&mut handle, Duration::from_millis(400), &mut events).await;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let _ = driver.close().await;

    let journal = Journal::open_with(
        &journal_dir,
        JournalOptions {
            fsync: FsyncPolicy::Never,
        },
    )?;
    let instance = events
        .first()
        .map(|obs| obs.instance_id.clone())
        .context("no observations")?;
    for obs in &events {
        journal.append(&instance, envelope(obs)).await?;
    }
    let stored = journal.read_range(&instance, U64(1), None).await?;
    let folded = fold_all(&stored);
    let snap = journal.snapshot(&instance).await?;

    let kinds: Vec<&str> = events.iter().map(kind_name).collect();
    let turn_done = events
        .iter()
        .filter(|o| lifecycle_status(o) == Some("turn_done"))
        .count();
    let failure = assert_script(kind, &events, &folded, turn_done);
    Ok(json!({
        "script": args.script,
        "ok": failure.is_none(),
        "failure": failure,
        "kinds": kinds,
        "turnDone": turn_done,
        "journalSeq": stored.len(),
        "transcriptEntries": snap.projections.transcript.entries.len(),
        "interactions": snap.projections.interaction.by_key.len(),
        "foldSeq": folded.transcript.as_of_seq.0,
        "snapshotSeq": snap.as_of_seq.0,
    }))
}

fn load_spec(workdir: &Path) -> Result<InstanceSpec> {
    let mut spec: InstanceSpec = serde_json::from_str(INSTANCE_SPEC)?;
    spec.cwd = workdir.canonicalize()?.to_string_lossy().into_owned();
    Ok(spec)
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001"
            .parse()
            .expect("profile id"),
        kind: ProviderKind::Anthropic,
        base_url: String::new(),
        delegation: remuda_driver::Delegation::None,
        secret_ref: None,
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn prompt(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: InputOrigin::Human,
        native_client_message_id: "m0-stub".into(),
    }))
}

fn pin_fake(path: &Path) -> Result<BinaryPin> {
    let abs = path
        .canonicalize()
        .with_context(|| format!("{}", path.display()))?;
    let bytes = fs::read(&abs)?;
    let hash = Sha256::digest(&bytes);
    let digest = Digest::try_from(format!("sha256:{hash:x}"))?;
    Ok(BinaryPin {
        abs_path: abs.to_string_lossy().into_owned(),
        version: "fake-claude".into(),
        sha256: digest,
    })
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

async fn maybe_answer(
    driver: &ClaudePrintDriver,
    kind: ScriptKind,
    obs: &Observation,
) -> Result<()> {
    let ObservationPayload::InteractionRequested(payload) = &obs.body else {
        return Ok(());
    };
    let interaction = &payload.interaction;
    match kind {
        ScriptKind::Approval => {
            let digest = match &interaction.request {
                InteractionRequest::Approval(req) => req.input_digest.clone(),
                _ => dummy_digest(),
            };
            driver
                .respond_interaction(
                    interaction.meta.id.clone(),
                    InteractionAnswer::Approval(Box::new(ApprovalAnswer {
                        option_id: "allow".into(),
                        input_digest: digest,
                    })),
                )
                .await?;
        }
        ScriptKind::AskUser => {
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
                    interaction.meta.id.clone(),
                    InteractionAnswer::Question(Box::new(QuestionAnswer { answers })),
                )
                .await?;
        }
        ScriptKind::Ok | ScriptKind::Workflow => {}
    }
    Ok(())
}

fn done(kind: ScriptKind, events: &[Observation]) -> bool {
    let turns = events
        .iter()
        .filter(|o| lifecycle_status(o) == Some("turn_done"))
        .count();
    match kind {
        ScriptKind::Workflow => turns >= 2,
        _ => turns >= 1,
    }
}

async fn drain(
    handle: &mut remuda_driver::RunHandle,
    window: Duration,
    events: &mut Vec<Observation>,
) {
    let deadline = tokio::time::Instant::now() + window;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, handle.recv()).await {
            Ok(Some(obs)) => events.push(obs),
            _ => break,
        }
    }
}

fn assert_script(
    kind: ScriptKind,
    events: &[Observation],
    folded: &remuda_journal::Projections,
    turn_done: usize,
) -> Option<String> {
    let names: Vec<&str> = events.iter().map(kind_name).collect();
    let has = |n: &str| names.contains(&n);
    match kind {
        ScriptKind::Ok => {
            if !has("lifecycle") {
                return Some(format!("ok: missing lifecycle in {names:?}"));
            }
            if !has("message") {
                return Some(format!("ok: missing message in {names:?}"));
            }
            if turn_done < 1 {
                return Some(format!("ok: expected turn_done, got {names:?}"));
            }
            if folded.transcript.entries.is_empty() {
                return Some("ok: empty folded transcript".into());
            }
        }
        ScriptKind::Approval => {
            if !has("interaction.requested") {
                return Some(format!("approval: missing interaction in {names:?}"));
            }
            if !has("tool_result") && turn_done < 1 {
                return Some(format!(
                    "approval: missing tool_result/turn_done in {names:?}"
                ));
            }
            if folded.interaction.by_key.is_empty() {
                return Some("approval: empty interaction fold".into());
            }
        }
        ScriptKind::AskUser => {
            let question = events.iter().any(|o| match &o.body {
                ObservationPayload::InteractionRequested(p) => {
                    p.interaction.kind == InteractionKind::Question
                }
                _ => false,
            });
            if !question {
                return Some(format!("askuser: missing question in {names:?}"));
            }
            if turn_done < 1 {
                return Some(format!("askuser: expected turn_done in {names:?}"));
            }
        }
        ScriptKind::Workflow => {
            if turn_done < 2 {
                return Some(format!(
                    "workflow: expected two turn_done, got {turn_done} {names:?}"
                ));
            }
            let wf = events.iter().any(|o| match &o.body {
                ObservationPayload::WorkflowRun(run) => {
                    !matches!(run.state, WorkflowState::Unknown)
                }
                _ => false,
            });
            if !wf && !has("workflow.run") {
                return Some(format!("workflow: missing workflow.run in {names:?}"));
            }
        }
    }
    None
}

fn kind_name(obs: &Observation) -> &'static str {
    match &obs.body {
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
        other => match other.kind() {
            remuda_protocol::ObservationKind::Message => "message",
            remuda_protocol::ObservationKind::Thought => "thought",
            remuda_protocol::ObservationKind::ToolCall => "tool_call",
            remuda_protocol::ObservationKind::ToolResult => "tool_result",
            remuda_protocol::ObservationKind::InteractionRequested => "interaction.requested",
            remuda_protocol::ObservationKind::InteractionAnswered => "interaction.answered",
            remuda_protocol::ObservationKind::InteractionExpired => "interaction.expired",
            remuda_protocol::ObservationKind::WorkflowRun => "workflow.run",
            remuda_protocol::ObservationKind::WorkflowPhase => "workflow.phase",
            remuda_protocol::ObservationKind::WorkflowMember => "workflow.member",
            remuda_protocol::ObservationKind::Lifecycle => "lifecycle",
            remuda_protocol::ObservationKind::Usage => "usage",
            remuda_protocol::ObservationKind::Artifact => "artifact",
            remuda_protocol::ObservationKind::Effort => "effort",
            remuda_protocol::ObservationKind::Model => "model",
            remuda_protocol::ObservationKind::Permission => "permission",
            remuda_protocol::ObservationKind::RawTty => "raw_tty",
            remuda_protocol::ObservationKind::Opaque => "opaque",
        },
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

fn dummy_digest() -> Digest {
    Digest::try_from(format!("sha256:{:0>64}", "a")).expect("digest")
}
