//! Herdr-backed `claude-pty` driver (plan M0-09).
//!
//! Starts a full [`LaunchRecipe`] in an isolated Herdr pane via `agent.start`,
//! never `--bare`. Restore rebuilds the pane with `--resume <session>` and does
//! not rely on Herdr's agent-resume argv. Structured identity comes from a
//! SessionStart hook overlay; screen status is `pane.agent_status_changed`.

use crate::binary::{BinaryPin, hash_bytes, pin_binary};
use crate::capabilities::{ADAPTER_VERSION, capability_snapshot};
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::materializer::{
    BinarySource, LaunchOrigin, MaterializeRequest, SessionAction, materialize,
};
use crate::profile::{EnvFileSecretBroker, ProviderProfile, SecretBroker};
use crate::recipe::{FileLifetime, FileRole, LaunchRecipe, MaterializedFile};
use async_trait::async_trait;
use remuda_herdr::{
    AgentPromptParams, AgentStartParams, AgentStatus, Client, EventKind, EventStream, HerdrServer,
    PaneSplitParams, SplitDirection, Subscription, TerminalObserver, WorkspaceCreateParams,
};
use remuda_protocol::{
    AgentKind, ClaudeRef, Completeness, ContentBlock, DeadlineSource, DeliveryState, Digest,
    DriverInput, DriverKind, EntityMeta, EventId, HerdrRef, HerdrRepresentation,
    HerdrServer as HerdrPin, HostId, Id, InstanceId, InstanceSpec, Interaction, InteractionAnswer,
    InteractionCarrier, InteractionId, InteractionKind, InteractionRequest, InteractionRequestKey,
    InteractionState, Knowledge, LifecyclePayload, LifecycleTopic, NativeLifecycle, NativeRef,
    NativeRequestKey, NativeTerminalFrame, Observation, ObservationPayload, ObservationSource,
    PtyBackend, PtyCarrier, QuestionField, QuestionInput, QuestionRequest, RawRef, Redaction,
    RunId, SchemaVersion, Severity, SourceChannel, SourceCursor, SourceDelivery, Timestamp,
    TranscriptRef, TtyOutput, TtyRepresentation, U64,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, info};

const HOOK_SCRIPT: &str = r#"#!/bin/sh
set -eu
out="$1"
raw="${out}.raw"
cat >"$raw"
if command -v python3 >/dev/null 2>&1; then
python3 - "$raw" "$out" <<'PY'
import json, sys
raw, out = sys.argv[1], sys.argv[2]
with open(raw, encoding="utf-8") as handle:
    data = json.load(handle)
payload = {
    "session_id": data.get("session_id") or data.get("sessionId"),
    "transcript_path": data.get("transcript_path") or data.get("transcriptPath"),
    "cwd": data.get("cwd"),
    "hook_event_name": data.get("hook_event_name") or data.get("hookEventName"),
}
with open(out, "w", encoding="utf-8") as handle:
    json.dump(payload, handle)
    handle.write("\n")
PY
else
  cp "$raw" "$out"
fi
"#;

/// Construction options for [`ClaudePtyDriver`].
pub struct ClaudePtyOptions {
    /// Provider profile used at materialize time.
    pub profile: ProviderProfile,
    /// Directory for 0700 launch files (settings overlay + SessionStart hook).
    pub launch_dir: PathBuf,
    /// Registered `CLAUDE_CONFIG_DIR`.
    pub native_home: PathBuf,
    /// Binary to pin (the `claude` executable; Herdr locates it by kind).
    pub binary: BinarySource,
    /// Launch origin; bot/dispatcher cannot request bypass (D-011).
    pub origin: LaunchOrigin,
    /// Isolated Herdr session name. Never `"default"` unless explicit.
    pub session_name: String,
    /// If set, API socket is `{socket_dir}/herdr.sock` (tests bind fake-herdr here).
    pub socket_dir: Option<PathBuf>,
    /// Override the `herdr` binary used for RPC-adjacent terminal observe.
    pub herdr_binary: Option<PathBuf>,
    /// Secret resolver. Defaults to [`EnvFileSecretBroker`].
    pub broker: Arc<dyn SecretBroker>,
    /// Extra env forwarded onto the Herdr workspace/pane (no secrets logged).
    pub extra_env: BTreeMap<String, String>,
    /// Override `--setting-sources`.
    pub setting_sources: Option<Vec<String>>,
    /// `agent.start` timeout in milliseconds.
    pub agent_start_timeout_ms: u64,
}

impl ClaudePtyOptions {
    /// Isolated `remuda-test` session, human origin, env/file broker.
    pub fn new(
        profile: ProviderProfile,
        launch_dir: PathBuf,
        native_home: PathBuf,
        binary: BinarySource,
    ) -> Self {
        Self {
            profile,
            launch_dir,
            native_home,
            binary,
            origin: LaunchOrigin::Human,
            session_name: "remuda-test".into(),
            socket_dir: None,
            herdr_binary: None,
            broker: Arc::new(EnvFileSecretBroker),
            extra_env: BTreeMap::new(),
            setting_sources: None,
            agent_start_timeout_ms: 120_000,
        }
    }
}

struct PtyLive {
    ctx: crate::claude_pty::ObsCtx,
    client: Client,
    pane_id: String,
    agent_name: String,
    recipe: LaunchRecipe,
    instance_id: InstanceId,
    host_id: HostId,
    journal_id: Id,
    run_id: RunId,
    session_id: String,
    native_store_id: Id,
    herdr_pin: HerdrPin,
    herdr_session: String,
    transcript_path: Arc<std::sync::Mutex<Option<String>>>,
    events: mpsc::Sender<Observation>,
    status_task: Option<JoinHandle<()>>,
    hook_task: Option<JoinHandle<()>>,
    tty_task: Option<JoinHandle<()>>,
    closed: bool,
}

/// Native Claude TUI hosted in a Herdr pane.
pub struct ClaudePtyDriver {
    resources: crate::pty_resource::PtyResources,
    options: ClaudePtyOptions,
    inner: Mutex<Option<PtyLive>>,
    last_recipe: Mutex<Option<LaunchRecipe>>,
    /// Last successfully materialized spec; `resume` re-materializes from this.
    last_spec: Mutex<Option<InstanceSpec>>,
    closed: AtomicBool,
    seq: Arc<AtomicU64>,
}

impl ClaudePtyDriver {
    /// Build a driver from explicit options.
    pub fn new(options: ClaudePtyOptions) -> Self {
        Self {
            resources: Default::default(),
            options,
            inner: Mutex::new(None),
            last_recipe: Mutex::new(None),
            last_spec: Mutex::new(None),
            closed: AtomicBool::new(false),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Last persisted recipe, if any. Contains no secrets or prompts.
    pub async fn persisted_recipe(&self) -> Option<LaunchRecipe> {
        self.last_recipe.lock().await.clone()
    }

    /// Transcript path recorded by the SessionStart hook, if it has fired.
    pub async fn session_transcript(&self) -> Option<std::path::PathBuf> {
        if let Some(inner) = self.inner.lock().await.as_ref()
            && let Ok(guard) = inner.transcript_path.lock()
            && let Some(path) = guard.as_ref()
            && !path.is_empty()
        {
            return Some(PathBuf::from(path));
        }
        read_session_meta_transcript(&self.options.launch_dir)
    }

    /// Open a read-only Herdr terminal observer for the live pane.
    pub async fn open_observer(&self) -> DriverResult<TerminalObserver> {
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
        if live.closed {
            return Err(DriverError::ControlUnavailable);
        }
        TerminalObserver::open(&live.client, &live.pane_id, 80, 24)
            .await
            .map_err(map_herdr)
    }

    async fn launch(&self, spec: InstanceSpec, session: SessionAction) -> DriverResult<RunHandle> {
        if spec.driver != DriverKind::ClaudePty {
            return Err(DriverError::InvalidLaunchSpec(
                "ClaudePtyDriver requires driverKind claude-pty".into(),
            ));
        }
        if self.options.origin == LaunchOrigin::Bot {
            reject_bot_bypass(&spec)?;
        }
        let session_id = session.session_id().to_string();
        let request = MaterializeRequest {
            spec: &spec,
            profile: &self.options.profile,
            launch_dir: self.options.launch_dir.clone(),
            native_home: self.options.native_home.clone(),
            session,
            launch_id: Id::new("launch")?,
            binary: self.options.binary.clone(),
            setting_sources: self.options.setting_sources.clone(),
            origin: self.options.origin,
        };
        let mut recipe = materialize(&request)?;
        apply_tty_bypass_flag(&mut recipe);
        refuse_bare(&recipe.argv)?;
        recipe = inject_session_start_hook(&mut recipe, &self.options.launch_dir)?;
        apply_tty_bypass_flag(&mut recipe);
        refuse_bare(&recipe.argv)?;
        *self.last_recipe.lock().await = Some(recipe.clone());
        *self.last_spec.lock().await = Some(spec.clone());

        let session_name = herdr_session_name(&spec, &self.options.session_name);
        if session_name == "default" && self.options.socket_dir.is_none() {
            return Err(DriverError::InvalidLaunchSpec(
                "refusing the default herdr session unless socket_dir is explicit".into(),
            ));
        }
        let server = HerdrServer::ensure(&session_name, self.options.socket_dir.clone())
            .await
            .map_err(map_herdr)?;
        let client = bind_client(&server, &self.options)?;
        let pong = client.ping().await.map_err(map_herdr)?;
        let herdr_pin = herdr_pin(&client, &pong.version, pong.protocol)?;

        let mut env = HashMap::new();
        env.insert("CLAUDE_CONFIG_DIR".into(), recipe.native_home.clone());
        for (key, value) in &self.options.extra_env {
            env.insert(key.clone(), value.clone());
        }

        let created = self
            .resources
            .create_workspace(
                &client,
                &session_name,
                WorkspaceCreateParams {
                    cwd: Some(recipe.cwd.clone()),
                    env: env.clone(),
                    focus: false,
                    label: Some(format!("remuda-{}", uuid::Uuid::now_v7())),
                    source_workspace_id: None,
                },
            )
            .await?;
        let split = client
            .pane_split(PaneSplitParams {
                direction: SplitDirection::Right,
                workspace_id: Some(created.workspace.workspace_id.clone()),
                target_pane_id: Some(created.root_pane.pane_id.clone()),
                ratio: None,
                cwd: Some(recipe.cwd.clone()),
                env,
                focus: false,
            })
            .await
            .map_err(map_herdr)?;
        let pane_id = split.pane.pane_id.clone();
        self.resources
            .record(&client, &session_name, &created, &pane_id)
            .await?;
        let agent_name = agent_name_for(&spec);

        let started = client
            .agent_start(AgentStartParams {
                name: agent_name.clone(),
                kind: "claude".into(),
                pane_id: pane_id.clone(),
                args: recipe.argv.clone(),
                timeout_ms: Some(self.options.agent_start_timeout_ms),
            })
            .await
            .map_err(map_herdr)?;
        refuse_bare(&started.argv)?;

        let native_session = started
            .agent
            .agent_session
            .as_ref()
            .map(|info| info.value.clone())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| session_id.clone());

        let stream = client
            .subscribe(vec![Subscription::pane_agent_status_changed(&pane_id)])
            .await
            .map_err(map_herdr)?;

        let instance_id = InstanceId::new();
        let run_id = RunId::new();
        let journal_id = Id::new("obj")?;
        let (tx, rx) = mpsc::channel(64);
        self.seq.store(0, Ordering::SeqCst);
        self.closed.store(false, Ordering::SeqCst);

        let mut ack = DriverAck::transport_written();
        ack.native_ids
            .insert("sessionId".into(), native_session.clone());
        ack.native_ids.insert("paneId".into(), pane_id.clone());
        ack.native_ids
            .insert("herdrSession".into(), session_name.clone());
        ack.native_ids
            .insert("agentName".into(), agent_name.clone());

        let ctx = ObsCtx {
            driver: DriverKind::ClaudePty,
            instance_id: instance_id.clone(),
            host_id: spec.host.clone(),
            journal_id: journal_id.clone(),
            run_id: run_id.clone(),
            session_id: native_session.clone(),
            pin_version: recipe.binary.version.clone(),
        };
        emit_obs(
            &tx,
            &self.seq,
            &ctx,
            SourceChannel::Herdr,
            Completeness::Structured,
            ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                NativeLifecycle {
                    topic: LifecycleTopic::Session,
                    native_name: "session".into(),
                    native_id: Knowledge::Known {
                        value: native_session.clone(),
                    },
                    status: Knowledge::Known {
                        value: format!("{:?}", started.agent.agent_status).to_ascii_lowercase(),
                    },
                    related_ids: BTreeMap::from([
                        ("paneId".into(), pane_id.clone()),
                        ("herdrSession".into(), session_name.clone()),
                    ]),
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                },
            )))),
        )
        .await?;

        let status_task = spawn_status_pump(stream, tx.clone(), ctx.clone(), Arc::clone(&self.seq));
        let hook_path = self.options.launch_dir.join("session-meta.json");
        let transcript_path = Arc::new(std::sync::Mutex::new(None));
        let hook_task = spawn_hook_watch(
            hook_path,
            tx.clone(),
            ctx.clone(),
            Arc::clone(&self.seq),
            Arc::clone(&transcript_path),
        );

        *self.inner.lock().await = Some(PtyLive {
            ctx,
            client,
            pane_id,
            agent_name,
            recipe: recipe.clone(),
            instance_id,
            host_id: spec.host.clone(),
            journal_id,
            run_id,
            session_id: native_session,
            native_store_id: spec.native_home.store_id.clone(),
            herdr_pin,
            herdr_session: session_name,
            transcript_path,
            events: tx,
            status_task: Some(status_task),
            hook_task: Some(hook_task),
            tty_task: None,
            closed: false,
        });

        info!(
            launch_id = %recipe.launch_id,
            "claude-pty agent.start dispatched"
        );
        Ok(RunHandle::new(recipe, ack, rx))
    }

    /// Native identity for the live pane, if started.
    pub async fn native_ref(&self) -> Option<NativeRef> {
        self.inner.lock().await.as_ref().map(native_ref_for)
    }
}

#[async_trait]
impl Driver for ClaudePtyDriver {
    fn track_pty_resources(&self, id: InstanceId, store: Arc<dyn crate::PtyResourceStore>) {
        self.resources.configure(id, store);
    }

    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        let pin = pin_source(&self.options.binary)?;
        Ok(capability_snapshot(
            DriverKind::ClaudePty,
            &pin,
            U64(1),
            U64(1),
        )?)
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        let session_id = uuid::Uuid::now_v7().to_string();
        match self.launch(spec, SessionAction::New { session_id }).await {
            Ok(handle) => Ok(handle),
            Err(error) => {
                if let Err(cleanup) = self.resources.close().await {
                    tracing::error!(%cleanup, "failed launch resource cleanup");
                }
                Err(error)
            }
        }
    }

    async fn attach(&self, native_ref: NativeRef) -> DriverResult<DriverAck> {
        let mut inner = self.inner.lock().await;
        let live = inner.as_mut().ok_or(DriverError::NativeSessionNotFound)?;
        if live.closed {
            return Err(DriverError::ControlUnavailable);
        }
        match &native_ref.session_id {
            Knowledge::Known { value } if value == &live.session_id || value.is_empty() => {}
            Knowledge::Known { .. } => {
                return Err(DriverError::NativeSessionNotFound);
            }
            Knowledge::Unknown { .. } | Knowledge::NotApplicable => {
                if native_ref.herdr.as_ref().map(|h| h.pane_id.as_str())
                    != Some(live.pane_id.as_str())
                {
                    return Err(DriverError::NativeSessionNotFound);
                }
            }
        }
        let pane_id = native_ref
            .herdr
            .as_ref()
            .map(|herdr| herdr.pane_id.clone())
            .unwrap_or_else(|| live.pane_id.clone());
        let observer = TerminalObserver::open(&live.client, &pane_id, 80, 24)
            .await
            .map_err(map_herdr)?;
        if let Some(task) = live.tty_task.take() {
            task.abort();
        }
        let ctx = ObsCtx {
            driver: DriverKind::ClaudePty,
            instance_id: live.instance_id.clone(),
            host_id: live.host_id.clone(),
            journal_id: live.journal_id.clone(),
            run_id: live.run_id.clone(),
            session_id: live.session_id.clone(),
            pin_version: live.recipe.binary.version.clone(),
        };
        live.tty_task = Some(spawn_tty_pump(
            observer,
            live.events.clone(),
            ctx,
            Arc::clone(&self.seq),
        ));
        let mut ack = DriverAck::transport_written();
        ack.native_ids.insert("paneId".into(), pane_id);
        ack.native_ids
            .insert("sessionId".into(), live.session_id.clone());
        Ok(ack)
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        let text = prompt_text(&input)?;
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
        if live.closed {
            return Err(DriverError::ControlUnavailable);
        }
        live.client
            .agent_prompt(AgentPromptParams {
                target: live.agent_name.clone(),
                text,
                wait: None,
            })
            .await
            .map_err(map_herdr)?;
        let mut ack = DriverAck::transport_written();
        ack.native_ids
            .insert("sessionId".into(), live.session_id.clone());
        ack.native_ids.insert("paneId".into(), live.pane_id.clone());
        Ok(ack)
    }

    async fn send_keys(&self, keys: Vec<String>) -> DriverResult<DriverAck> {
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
        if live.closed {
            return Err(DriverError::ControlUnavailable);
        }
        live.client
            .agent_send_keys(live.agent_name.clone(), keys)
            .await
            .map_err(map_herdr)?;
        Ok(DriverAck::transport_written())
    }

    async fn cancel(&self) -> DriverResult<DriverAck> {
        self.send_keys(vec!["esc".into()]).await
    }

    async fn respond_interaction(
        &self,
        _id: InteractionId,
        _answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        Err(DriverError::CapabilityUnsupported(
            "claude-pty screen-derived interactions are not answerable; use the native TTY".into(),
        ))
    }

    async fn close(&self) -> DriverResult<DriverAck> {
        self.closed.store(true, Ordering::SeqCst);
        let mut inner = self.inner.lock().await;
        let Some(live) = inner.as_mut() else {
            drop(inner);
            self.resources.close().await?;
            return Ok(DriverAck::not_dispatched());
        };
        if live.closed {
            drop(inner);
            self.resources.close().await?;
            return Ok(DriverAck::not_dispatched());
        }
        live.closed = true;
        if let Some(task) = live.status_task.take() {
            task.abort();
        }
        if let Some(task) = live.hook_task.take() {
            task.abort();
        }
        if let Some(task) = live.tty_task.take() {
            task.abort();
        }
        let events = live.events.clone();
        let ctx = live.ctx.clone();
        drop(inner);
        self.resources.close().await?;
        crate::claude_pty::emit_pty_closed(&events, &self.seq, &ctx).await?;
        self.inner.lock().await.take();
        Ok(DriverAck::not_dispatched())
    }

    async fn resume(&self, native_ref: NativeRef) -> DriverResult<RunHandle> {
        let session_id = match &native_ref.session_id {
            Knowledge::Known { value } if !value.is_empty() => value.clone(),
            _ => native_ref
                .claude
                .as_ref()
                .map(|claude| claude.session_id.clone())
                .filter(|value| !value.is_empty())
                .ok_or(DriverError::NativeSessionNotFound)?,
        };
        let mut spec = self
            .last_spec
            .lock()
            .await
            .clone()
            .ok_or(DriverError::NativeSessionNotFound)?;
        spec.driver = DriverKind::ClaudePty;
        spec.host = native_ref.host_id.clone();
        spec.kind = AgentKind::Claude;
        if let Some(herdr) = &native_ref.herdr {
            spec.carrier = remuda_protocol::CarrierSpec::Pty(Box::new(PtyCarrier {
                backend: PtyBackend::Herdr,
                server: herdr.server.clone(),
                session: herdr.session.clone(),
            }));
        }
        self.launch(spec, SessionAction::Resume { session_id })
            .await
    }
}

#[derive(Clone)]
pub(crate) struct ObsCtx {
    pub(crate) driver: DriverKind,
    pub(crate) instance_id: InstanceId,
    pub(crate) host_id: HostId,
    pub(crate) journal_id: Id,
    pub(crate) run_id: RunId,
    pub(crate) session_id: String,
    pub(crate) pin_version: String,
}

fn spawn_status_pump(
    mut stream: EventStream,
    tx: mpsc::Sender<Observation>,
    ctx: ObsCtx,
    seq: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(item) = stream.next_event().await {
            let event = match item {
                Ok(event) => event,
                Err(err) => {
                    debug!(error = %err, "herdr subscribe ended");
                    break;
                }
            };
            if event.kind != EventKind::PaneAgentStatusChanged {
                continue;
            }
            let Some(status) = event.agent_status() else {
                continue;
            };
            let payloads = status_payloads(&ctx, status);
            for (channel, completeness, payload) in payloads {
                if emit_obs(&tx, &seq, &ctx, channel, completeness, payload)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    })
}

fn read_session_meta_transcript(launch_dir: &Path) -> Option<PathBuf> {
    let body = std::fs::read_to_string(launch_dir.join("session-meta.json")).ok()?;
    let value: Value = serde_json::from_str(&body).ok()?;
    value
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn spawn_hook_watch(
    path: PathBuf,
    tx: mpsc::Sender<Observation>,
    ctx: ObsCtx,
    seq: Arc<AtomicU64>,
    transcript_slot: Arc<std::sync::Mutex<Option<String>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Live Claude startup (trust UI + SessionStart) can exceed 10s.
        for _ in 0..900 {
            if let Ok(body) = tokio::fs::read_to_string(&path).await
                && let Ok(value) = serde_json::from_str::<Value>(&body)
            {
                let session = value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or(&ctx.session_id);
                let transcript = value
                    .get("transcript_path")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !transcript.is_empty()
                    && let Ok(mut slot) = transcript_slot.lock()
                {
                    *slot = Some(transcript.to_string());
                }
                let mut related = BTreeMap::new();
                if !transcript.is_empty() {
                    related.insert("transcriptPath".into(), transcript.to_string());
                }
                let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(
                    Box::new(NativeLifecycle {
                        topic: LifecycleTopic::Hook,
                        native_name: "SessionStart".into(),
                        native_id: Knowledge::Known {
                            value: session.to_string(),
                        },
                        status: Knowledge::Known {
                            value: "recorded".into(),
                        },
                        related_ids: related,
                        data_ref: None,
                        severity: Severity::Info,
                        affects_completion: false,
                    }),
                )));
                let _ = emit_obs(
                    &tx,
                    &seq,
                    &ctx,
                    SourceChannel::Hook,
                    Completeness::Structured,
                    payload,
                )
                .await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
}

fn spawn_tty_pump(
    mut observer: TerminalObserver,
    tx: mpsc::Sender<Observation>,
    ctx: ObsCtx,
    seq: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(frame) = observer.next_frame().await {
            let Ok(frame) = frame else {
                break;
            };
            let stream_id = match Id::new("tty") {
                Ok(id) => id,
                Err(_) => break,
            };
            let stream_epoch = match Id::new("epoch") {
                Ok(id) => id,
                Err(_) => break,
            };
            let object_id = match Id::new("obj") {
                Ok(id) => id,
                Err(_) => break,
            };
            let digest = match hash_bytes(&frame.bytes) {
                Ok(digest) => digest,
                Err(_) => match dummy_digest() {
                    Ok(digest) => digest,
                    Err(_) => break,
                },
            };
            let payload = ObservationPayload::RawTty(Box::new(
                remuda_protocol::RawTtyPayload::Output(Box::new(TtyOutput {
                    stream_id,
                    stream_epoch,
                    representation: TtyRepresentation::RenderedAnsi,
                    offset: U64(frame.seq),
                    byte_length: U64(frame.bytes.len() as u64),
                    data_ref: RawRef {
                        object_id,
                        offset: U64(0),
                        length: U64(frame.bytes.len() as u64),
                        digest,
                        media_type: "application/vnd.remuda.tty-ansi".into(),
                        redaction: Redaction::None,
                    },
                    native_frame: Knowledge::Known {
                        value: NativeTerminalFrame {
                            seq: U64(frame.seq),
                            width: frame.width,
                            height: frame.height,
                            full: frame.full,
                        },
                    },
                })),
            ));
            if emit_obs(
                &tx,
                &seq,
                &ctx,
                SourceChannel::Pty,
                Completeness::Partial,
                payload,
            )
            .await
            .is_err()
            {
                break;
            }
        }
    })
}

fn status_payloads(
    ctx: &ObsCtx,
    status: AgentStatus,
) -> Vec<(SourceChannel, Completeness, ObservationPayload)> {
    let label = match status {
        AgentStatus::Working => "working",
        AgentStatus::Idle => "idle",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "idle",
        AgentStatus::Unknown => "unknown",
    };
    let status_completeness = if status == AgentStatus::Blocked {
        Completeness::ScreenDerived
    } else {
        Completeness::Partial
    };
    let mut out = vec![(
        SourceChannel::Herdr,
        status_completeness,
        ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic: LifecycleTopic::Turn,
                native_name: "agent_status".into(),
                native_id: Knowledge::Known {
                    value: ctx.session_id.clone(),
                },
                status: Knowledge::Known {
                    value: label.into(),
                },
                related_ids: BTreeMap::new(),
                data_ref: None,
                severity: Severity::Info,
                affects_completion: false,
            },
        )))),
    )];
    if status == AgentStatus::Blocked
        && let Ok(interaction) = screen_block_interaction(ctx)
    {
        out.push((
            SourceChannel::Herdr,
            Completeness::ScreenDerived,
            ObservationPayload::InteractionRequested(Box::new(
                remuda_protocol::InteractionRequestedPayload { interaction },
            )),
        ));
    }
    out
}

fn screen_block_interaction(ctx: &ObsCtx) -> DriverResult<Interaction> {
    let ts = now_ts()?;
    Ok(Interaction {
        meta: EntityMeta {
            id: InteractionId::new(),
            revision: U64(1),
            created_at: ts.clone(),
            updated_at: ts,
        },
        instance_id: ctx.instance_id.clone(),
        run_id: Some(ctx.run_id.clone()),
        host_id: ctx.host_id.clone(),
        kind: InteractionKind::Question,
        request_key: InteractionRequestKey {
            native: NativeRequestKey::None,
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            connection_epoch: Id::new("epoch")?,
        },
        request_version: U64(1),
        state: InteractionState::Pending,
        blocking: true,
        answerable: false,
        carrier: InteractionCarrier::NativeTty,
        request: InteractionRequest::Question(Box::new(QuestionRequest {
            title: "Native TTY is blocked".into(),
            fields: vec![QuestionField {
                id: "screen".into(),
                title: "Herdr agent_status=blocked".into(),
                description: Some(
                    "Screen-derived; answerable=false. Do not send keys by coordinate.".into(),
                ),
                input: QuestionInput::Text,
                required: false,
                options: vec![],
                allow_free_text: false,
                sensitive: false,
            }],
        })),
        deadline: Knowledge::Unknown {
            reason: "screen-derived".into(),
            evidence_event_ids: vec![],
        },
        deadline_source: DeadlineSource::Native,
        answer: Knowledge::NotApplicable,
        delivery: DeliveryState::NotSent,
        resolution: Knowledge::Unknown {
            reason: "pending".into(),
            evidence_event_ids: vec![],
        },
    })
}

async fn emit_obs(
    tx: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &ObsCtx,
    channel: SourceChannel,
    completeness: Completeness,
    payload: ObservationPayload,
) -> DriverResult<()> {
    let n = seq.fetch_add(1, Ordering::SeqCst) + 1;
    tx.send(build_observation(ctx, n, channel, completeness, payload)?)
        .await
        .map_err(|_| DriverError::ControlUnavailable)?;
    Ok(())
}

pub(crate) fn build_observation(
    ctx: &ObsCtx,
    seq: u64,
    channel: SourceChannel,
    completeness: Completeness,
    payload: ObservationPayload,
) -> DriverResult<Observation> {
    Ok(Observation {
        schema_version: SchemaVersion,
        event_id: EventId::new(),
        journal_id: ctx.journal_id.clone(),
        instance_id: ctx.instance_id.clone(),
        run_id: Some(ctx.run_id.clone()),
        host_id: ctx.host_id.clone(),
        process_generation: U64(1),
        run_generation: Some(U64(1)),
        seq: U64(seq),
        observed_at: now_ts()?,
        native_at: Knowledge::Unknown {
            reason: "herdr-clock".into(),
            evidence_event_ids: vec![],
        },
        source: ObservationSource {
            driver_kind: ctx.driver,
            driver_version: ctx.pin_version.clone(),
            adapter_version: ADAPTER_VERSION.into(),
            channel,
            delivery: SourceDelivery::Live,
            native_session_id: Knowledge::Known {
                value: ctx.session_id.clone(),
            },
            native_turn_id: Knowledge::Unknown {
                reason: "pty-no-turn-id".into(),
                evidence_event_ids: vec![],
            },
            native_agent_id: Knowledge::NotApplicable,
            native_item_id: Knowledge::Unknown {
                reason: "pty-no-item".into(),
                evidence_event_ids: vec![],
            },
            native_event_id: Knowledge::Unknown {
                reason: "pty-no-native-event".into(),
                evidence_event_ids: vec![],
            },
            native_request_id: NativeRequestKey::None,
            source_cursor: SourceCursor::Runtime(Box::new(remuda_protocol::RuntimeCursor {
                ledger_revision: U64(seq),
            })),
        },
        completeness,
        raw_ref: None,
        evidence_event_ids: vec![],
        body: payload,
    })
}

pub(crate) fn now_ts() -> DriverResult<Timestamp> {
    let now = time::OffsetDateTime::now_utc();
    let date = now.date();
    let (hour, minute, second) = now.time().as_hms();
    let formatted = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        hour,
        minute,
        second,
        now.millisecond()
    );
    Timestamp::try_from(formatted).map_err(DriverError::Protocol)
}

pub(crate) fn dummy_digest() -> DriverResult<Digest> {
    Digest::try_from(
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
    )
    .map_err(DriverError::Protocol)
}

pub(crate) fn prompt_text(input: &DriverInput) -> DriverResult<String> {
    match input {
        DriverInput::Prompt(prompt) => blocks_to_text(&prompt.blocks),
        DriverInput::Steer(_) => Err(DriverError::CapabilityUnsupported(
            "claude-pty does not accept structured steer".into(),
        )),
        DriverInput::ModelSwitch(_) => Err(DriverError::CapabilityUnknown(
            "claude-pty model-switch is native TUI only".into(),
        )),
    }
}

fn blocks_to_text(blocks: &[ContentBlock]) -> DriverResult<String> {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            out.push_str(&text.text);
        }
    }
    if out.is_empty() {
        return Err(DriverError::InvalidLaunchSpec(
            "prompt has no text block".into(),
        ));
    }
    Ok(out)
}

pub(crate) fn refuse_bare(argv: &[String]) -> DriverResult<()> {
    if argv.iter().any(|token| {
        crate::flags::is_banned_flag_token(token)
            || matches!(
                token.as_str(),
                "--bare" | "--safe-mode" | "--no-session-persistence" | "--continue"
            )
            || token.starts_with("--bare=")
            || token.starts_with("--safe-mode=")
            || token.starts_with("--no-session-persistence=")
            || token.starts_with("--continue=")
    }) {
        return Err(DriverError::NativeFeatureDisabled(
            "refusing prohibited flag on claude-pty/bg argv".into(),
        ));
    }
    Ok(())
}

pub(crate) fn apply_tty_bypass_flag(recipe: &mut LaunchRecipe) {
    const TTY: &str = "--dangerously-skip-permissions";
    const PRINT: &str = "--allow-dangerously-skip-permissions";
    recipe.argv.retain(|token| token != PRINT);
    recipe.permission.extra_flags.retain(|token| token != PRINT);
    if recipe
        .permission
        .extra_flags
        .iter()
        .any(|token| token == TTY)
        && !recipe.argv.iter().any(|token| token == TTY)
    {
        recipe.argv.push(TTY.into());
    }
}

pub(crate) fn argv_for_resume(argv: &[String], session_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut index = 0;
    let mut replaced = false;
    while index < argv.len() {
        let token = &argv[index];
        if token == "--session-id" || token == "--resume" || token == "--continue" {
            index += 1;
            if index < argv.len() && !argv[index].starts_with('-') {
                index += 1;
            }
            if !replaced {
                out.push("--resume".into());
                out.push(session_id.to_string());
                replaced = true;
            }
            continue;
        }
        if token.starts_with("--session-id=")
            || token.starts_with("--resume=")
            || token.starts_with("--continue=")
        {
            if !replaced {
                out.push("--resume".into());
                out.push(session_id.to_string());
                replaced = true;
            }
            index += 1;
            continue;
        }
        out.push(token.clone());
        index += 1;
    }
    if !replaced {
        out.push("--resume".into());
        out.push(session_id.to_string());
    }
    out
}

pub(crate) fn strip_named_flags(argv: &[String], names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        let token = &argv[index];
        let matched = names.iter().any(|name| {
            token == name
                || token
                    .strip_prefix(*name)
                    .is_some_and(|rest| rest.starts_with('='))
        });
        if matched {
            if !token.contains('=') {
                index += 1;
                if index < argv.len() && !argv[index].starts_with('-') {
                    index += 1;
                }
            } else {
                index += 1;
            }
            continue;
        }
        out.push(token.clone());
        index += 1;
    }
    out
}

pub(crate) fn inject_session_start_hook(
    recipe: &mut LaunchRecipe,
    launch_dir: &Path,
) -> DriverResult<LaunchRecipe> {
    std::fs::create_dir_all(launch_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(launch_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let hook_path = launch_dir.join("session-start.sh");
    let meta_path = launch_dir.join("session-meta.json");
    write_private(&hook_path, HOOK_SCRIPT.as_bytes(), 0o700)?;
    let command = format!(
        "{} {}",
        shell_single_quote(&hook_path.to_string_lossy()),
        shell_single_quote(&meta_path.to_string_lossy())
    );
    let settings_path = launch_dir.join("settings.json");
    let mut settings = if settings_path.exists() {
        serde_json::from_slice(&std::fs::read(&settings_path)?)?
    } else {
        json!({})
    };
    merge_session_start_hook(&mut settings, &command)?;
    if let Some(object) = settings.as_object_mut() {
        object
            .entry("bypassPermissionsModeAccepted")
            .or_insert(json!(true));
    }
    let bytes = serde_json::to_vec_pretty(&settings)?;
    write_private(&settings_path, &bytes, 0o600)?;
    let digest = hash_bytes(&bytes)?;
    if let Some(file) = recipe
        .materialized_files
        .iter_mut()
        .find(|file| file.role == FileRole::Settings)
    {
        file.path = settings_path.to_string_lossy().into_owned();
        file.content_digest = digest.clone();
    } else {
        recipe.materialized_files.push(MaterializedFile {
            path: settings_path.to_string_lossy().into_owned(),
            role: FileRole::Settings,
            mode: "0600".into(),
            content_digest: digest.clone(),
            lifetime: FileLifetime::Launch,
        });
    }
    recipe.audit.settings_digest = Some(digest);
    ensure_settings_flag(&mut recipe.argv, &settings_path);
    ensure_setting_sources(&mut recipe.argv)?;
    recipe.audit.redacted_argv = recipe
        .argv
        .iter()
        .map(|token| {
            if token == &settings_path.to_string_lossy() {
                "<settings>".into()
            } else {
                token.clone()
            }
        })
        .collect();
    Ok(recipe.clone())
}

fn merge_session_start_hook(settings: &mut Value, command: &str) -> DriverResult<()> {
    let object = settings.as_object_mut().ok_or_else(|| {
        DriverError::SettingsIsolationUnavailable("settings overlay is not an object".into())
    })?;
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    let hooks_object = hooks.as_object_mut().ok_or_else(|| {
        DriverError::SettingsIsolationUnavailable("hooks overlay is not an object".into())
    })?;
    let session_start = hooks_object
        .entry("SessionStart")
        .or_insert_with(|| json!([]));
    let matchers = session_start.as_array_mut().ok_or_else(|| {
        DriverError::SettingsIsolationUnavailable(
            "SessionStart hooks overlay is not an array".into(),
        )
    })?;
    let already = matchers.iter().any(|matcher| {
        matcher
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|hook| hook.get("command").and_then(Value::as_str) == Some(command))
            })
    });
    if !already {
        matchers.push(json!({
            "hooks": [{
                "type": "command",
                "command": command
            }]
        }));
    }
    Ok(())
}

fn ensure_settings_flag(argv: &mut Vec<String>, settings_path: &Path) {
    let path = settings_path.to_string_lossy().into_owned();
    if argv.windows(2).any(|pair| pair[0] == "--settings")
        || argv.iter().any(|token| token.starts_with("--settings="))
    {
        if let Some(index) = argv.iter().position(|token| token == "--settings")
            && let Some(value) = argv.get_mut(index + 1)
        {
            *value = path;
        }
        return;
    }
    argv.push("--settings".into());
    argv.push(path);
}

fn ensure_setting_sources(argv: &mut Vec<String>) -> DriverResult<()> {
    if let Some(index) = argv.iter().position(|token| token == "--setting-sources") {
        let value = argv.get(index + 1).map(String::as_str).unwrap_or("");
        if value.trim().is_empty() || value.starts_with('-') {
            return Err(DriverError::NativeFeatureDisabled(
                "empty --setting-sources is prohibited".into(),
            ));
        }
        return Ok(());
    }
    if let Some(token) = argv
        .iter()
        .find(|token| token.starts_with("--setting-sources="))
    {
        let value = token.trim_start_matches("--setting-sources=");
        if value.trim().is_empty() {
            return Err(DriverError::NativeFeatureDisabled(
                "empty --setting-sources is prohibited".into(),
            ));
        }
        return Ok(());
    }
    argv.push("--setting-sources".into());
    argv.push("user,project,local".into());
    Ok(())
}

fn write_private(path: &Path, contents: &[u8], mode: u32) -> DriverResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    let _ = mode;
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn bind_client(server: &HerdrServer, options: &ClaudePtyOptions) -> DriverResult<Client> {
    let mut client = Client::connect(server.socket_path()).with_timeout(Duration::from_secs(30));
    if options.socket_dir.is_none() {
        client = client.with_session_name(server.session_name());
    }
    if let Some(binary) = &options.herdr_binary {
        client = client.with_binary(binary);
    }
    Ok(client)
}

fn herdr_pin(client: &Client, version: &str, protocol: u32) -> DriverResult<HerdrPin> {
    let path = client.binary();
    let digest = crate::binary::hash_file(path).or_else(|_| dummy_digest())?;
    Ok(HerdrPin {
        binary_path: path.to_string_lossy().into_owned(),
        version: version.to_string(),
        digest,
        protocol_version: protocol.to_string(),
        server_identity: Id::new("obj")?,
        server_epoch: Id::new("epoch")?,
        representation: HerdrRepresentation::RenderedAnsi,
    })
}

fn herdr_session_name(spec: &InstanceSpec, fallback: &str) -> String {
    if let remuda_protocol::CarrierSpec::Pty(carrier) = &spec.carrier
        && !carrier.session.is_empty()
    {
        return carrier.session.clone();
    }
    fallback.to_string()
}

fn agent_name_for(spec: &InstanceSpec) -> String {
    let raw = spec.host.as_id().as_str().to_string();
    let suffix = raw
        .rsplit('_')
        .next()
        .unwrap_or("pty")
        .chars()
        .take(8)
        .collect::<String>();
    format!("remuda-{suffix}")
}

fn pin_source(source: &BinarySource) -> DriverResult<BinaryPin> {
    match source {
        BinarySource::Pinned(pin) => Ok(pin.clone()),
        BinarySource::Path(path) => pin_binary(path),
        BinarySource::Command(name) => pin_binary(name),
    }
}

fn reject_bot_bypass(spec: &InstanceSpec) -> DriverResult<()> {
    if let remuda_protocol::PermissionMode::Claude(claude) = &spec.permission_mode
        && matches!(
            claude.mode,
            remuda_protocol::ClaudePermissionMode::BypassPermissions
                | remuda_protocol::ClaudePermissionMode::DontAsk
        )
    {
        return Err(DriverError::BypassNotAllowedForBot);
    }
    Ok(())
}

fn native_ref_for(live: &PtyLive) -> NativeRef {
    let recorded = live
        .transcript_path
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let transcript = match recorded {
        Some(path) => match Id::new("obj") {
            Ok(object_id) => Knowledge::Known {
                value: TranscriptRef {
                    object_id,
                    source_path: path,
                },
            },
            Err(_) => Knowledge::Unknown {
                reason: "transcript-id".into(),
                evidence_event_ids: vec![],
            },
        },
        None => Knowledge::Unknown {
            reason: "session-start-pending".into(),
            evidence_event_ids: vec![],
        },
    };
    NativeRef {
        host_id: live.host_id.clone(),
        native_store_id: live.native_store_id.clone(),
        kind: AgentKind::Claude,
        session_id: Knowledge::Known {
            value: live.session_id.clone(),
        },
        transcript,
        codex: None,
        acp: None,
        claude: Some(ClaudeRef {
            session_id: live.session_id.clone(),
        }),
        claude_bg: None,
        agy: None,
        herdr: Some(HerdrRef {
            server: live.herdr_pin.clone(),
            session: live.herdr_session.clone(),
            pane_id: live.pane_id.clone(),
        }),
    }
}

pub(crate) fn map_herdr(err: remuda_herdr::Error) -> DriverError {
    match err {
        remuda_herdr::Error::Io(error) => DriverError::Io(error),
        remuda_herdr::Error::Json(error) => DriverError::Json(error),
        remuda_herdr::Error::BinaryNotFound => DriverError::BinaryNotFound(PathBuf::from("herdr")),
        remuda_herdr::Error::DefaultSessionGuard { socket } => DriverError::InvalidLaunchSpec(
            format!("refusing default herdr socket {}", socket.display()),
        ),
        remuda_herdr::Error::Api {
            code, message: _, ..
        } if code == "agent_not_ready" => DriverError::ControlUnavailable,
        other => DriverError::InvalidLaunchSpec(other.to_string()),
    }
}

/// Shared observation context used by the bg driver.
pub(crate) fn obs_ctx(
    driver: DriverKind,
    instance_id: InstanceId,
    host_id: HostId,
    journal_id: Id,
    run_id: RunId,
    session_id: String,
    pin_version: String,
) -> ObsCtx {
    ObsCtx {
        driver,
        instance_id,
        host_id,
        journal_id,
        run_id,
        session_id,
        pin_version,
    }
}

pub(crate) async fn emit_pty_closed(
    tx: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &ObsCtx,
) -> DriverResult<()> {
    let emission = emit_on(
        tx,
        seq,
        ctx,
        SourceChannel::Runtime,
        Completeness::Structured,
        ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic: LifecycleTopic::Session,
                native_name: "carrier_closed".into(),
                native_id: Knowledge::NotApplicable,
                status: Knowledge::Known {
                    value: "exited".into(),
                },
                related_ids: BTreeMap::from([("reason".into(), "owner-forced-stop".into())]),
                data_ref: None,
                severity: Severity::Info,
                affects_completion: false,
            },
        )))),
    );
    // The Node emits its durable exited entity after close returns. A detached or
    // stalled optional observer must not hold successful resource cleanup open.
    match tokio::time::timeout(Duration::from_millis(250), emission).await {
        Ok(result) if !tx.is_closed() => result,
        _ => Ok(()),
    }
}

pub(crate) async fn emit_on(
    tx: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &ObsCtx,
    channel: SourceChannel,
    completeness: Completeness,
    payload: ObservationPayload,
) -> DriverResult<()> {
    emit_obs(tx, seq, ctx, channel, completeness, payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_argv_replaces_session_id_and_never_continue() {
        let argv = vec![
            "--permission-mode".into(),
            "default".into(),
            "--session-id".into(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        ];
        let resumed = argv_for_resume(&argv, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        assert!(resumed.contains(&"--resume".into()));
        assert!(resumed.contains(&"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into()));
        assert!(!resumed.iter().any(|token| token == "--continue"));
        assert!(!resumed.iter().any(|token| token == "--session-id"));
        assert!(!resumed.iter().any(|token| token == "--bare"));
    }

    #[test]
    fn refuse_bare_rejects_prohibited_tokens() {
        assert!(refuse_bare(&["--model".into(), "haiku".into()]).is_ok());
        assert!(refuse_bare(&["--bare".into()]).is_err());
        assert!(refuse_bare(&["--continue".into()]).is_err());
    }
}

/// Mapping / gating helpers used by `tests/claude_pty_review.rs`.
#[doc(hidden)]
pub mod review {
    use super::*;

    /// Final argv check for `--bare` / `--continue` / banned tokens.
    pub fn refuse_argv(argv: &[String]) -> DriverResult<()> {
        refuse_bare(argv)
    }

    /// Rebuild resume argv from a persisted recipe, keeping `--settings`.
    pub fn resume_argv(argv: &[String], session_id: &str) -> Vec<String> {
        argv_for_resume(argv, session_id)
    }

    /// Append a SessionStart command without replacing other hook events.
    pub fn merge_hooks(settings: &mut Value, command: &str) -> DriverResult<()> {
        merge_session_start_hook(settings, command)
    }

    /// Write the launch-dir SessionStart overlay and keep `--settings`.
    pub fn inject_hook(recipe: &mut LaunchRecipe, launch_dir: &Path) -> DriverResult<LaunchRecipe> {
        inject_session_start_hook(recipe, launch_dir)
    }

    /// Map print-style bypass onto the TTY flag.
    pub fn apply_bypass(recipe: &mut LaunchRecipe) {
        apply_tty_bypass_flag(recipe);
    }

    /// Default `--setting-sources user,project,local` when missing; reject empty.
    pub fn ensure_sources(argv: &mut Vec<String>) -> DriverResult<()> {
        ensure_setting_sources(argv)
    }

    /// Screen-derived blocked payloads (lifecycle + interaction).
    pub fn blocked_status_payloads() -> DriverResult<Vec<(Completeness, ObservationPayload)>> {
        let ctx = obs_ctx(
            DriverKind::ClaudePty,
            InstanceId::new(),
            HostId::new(),
            Id::new("obj")?,
            RunId::new(),
            "review-session".into(),
            "review".into(),
        );
        Ok(status_payloads(&ctx, AgentStatus::Blocked)
            .into_iter()
            .map(|(_, completeness, payload)| (completeness, payload))
            .collect())
    }

    /// Driver-level bot/dontAsk/bypass gate.
    pub fn reject_bot(spec: &InstanceSpec) -> DriverResult<()> {
        reject_bot_bypass(spec)
    }

    /// Strip named flags (`--session-id`, `--cwd`, …).
    pub fn strip_flags(argv: &[String], names: &[&str]) -> Vec<String> {
        strip_named_flags(argv, names)
    }
}
