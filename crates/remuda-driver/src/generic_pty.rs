//! Herdr-backed `generic-pty` driver (D-014 / D0-1).
//!
//! Runs a registered CLI in a Herdr pane using a per-kind preset (binary, yolo
//! argv, herdr `--kind`, idle/blocked mapping). Structured journal hooks are
//! only injected for Claude kinds.

use crate::binary::{BinaryPin, pin_binary};
use crate::capabilities::capability_snapshot;
use crate::claude_pty::{
    dummy_digest, emit_on, inject_session_start_hook, map_herdr, obs_ctx, prompt_text, refuse_bare,
};
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::materializer::{
    BinarySource, LaunchOrigin, MaterializeRequest, SessionAction, materialize,
};
use crate::profile::{EnvFileSecretBroker, ProviderProfile, SecretBroker};
use crate::recipe::LaunchRecipe;
use async_trait::async_trait;
use remuda_herdr::{
    AgentPromptParams, AgentReadParams, AgentStartParams, AgentStatus, AgentWaitParams, Client,
    EventKind, EventStream, HerdrServer, PaneReadParams, PaneSplitParams, ReadFormat, ReadSource,
    SplitDirection, Subscription, WorkspaceCreateParams,
};
use remuda_protocol::{
    AgentKind, Completeness, DriverInput, DriverKind, HerdrRepresentation, HerdrServer as HerdrPin,
    HostId, Id, InstanceId, InstanceSpec, InteractionAnswer, InteractionId, Knowledge,
    LifecyclePayload, LifecycleTopic, NativeLifecycle, NativeRef, Observation, ObservationPayload,
    RunId, Severity, SourceChannel, U64,
};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::info;

/// Last N screen lines stored on each journal snapshot.
const SCREEN_SNAPSHOT_LINES: usize = 80;
/// How long [`Driver::wait_control`] waits for a live pane.
const CONTROL_READY_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-kind PTY launch conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindPreset {
    /// Product id (`claude`, `codex`, `grok`, `agy`, `gemini`).
    pub id: &'static str,
    /// Default executable on PATH.
    pub binary: &'static str,
    /// Herdr `agent.start --kind`.
    pub herdr_kind: &'static str,
    /// Non-interactive / yolo argv appended when missing.
    pub yolo_argv: &'static [&'static str],
    /// Optional CLI flag that receives the herdr agent name.
    pub name_flag: Option<&'static str>,
    /// Whether SessionStart journal hooks are injected.
    pub journals: bool,
    /// Treat herdr `done` as wait-until idle.
    pub done_means_idle: bool,
}

/// Built-in presets for dogfood kinds.
pub const PRESETS: &[KindPreset] = &[
    KindPreset {
        id: "claude",
        binary: "claude",
        herdr_kind: "claude",
        yolo_argv: &["--dangerously-skip-permissions"],
        name_flag: None,
        journals: true,
        done_means_idle: true,
    },
    KindPreset {
        id: "codex",
        binary: "codex",
        herdr_kind: "codex",
        yolo_argv: &["--dangerously-bypass-approvals-and-sandbox"],
        name_flag: None,
        journals: false,
        done_means_idle: true,
    },
    KindPreset {
        id: "grok",
        binary: "grok",
        herdr_kind: "grok",
        yolo_argv: &["--always-approve"],
        name_flag: None,
        journals: false,
        done_means_idle: true,
    },
    KindPreset {
        id: "agy",
        binary: "agy",
        herdr_kind: "agy",
        yolo_argv: &["--dangerously-skip-permissions"],
        name_flag: None,
        journals: false,
        done_means_idle: true,
    },
    KindPreset {
        id: "gemini",
        binary: "gemini",
        herdr_kind: "gemini",
        yolo_argv: &["--yolo"],
        name_flag: None,
        journals: false,
        done_means_idle: true,
    },
];

/// Look up a preset by product id.
pub fn preset_by_id(id: &str) -> Option<&'static KindPreset> {
    PRESETS
        .iter()
        .find(|preset| preset.id.eq_ignore_ascii_case(id))
}

/// Look up a preset from [`AgentKind`] (and optional spec args for gemini).
pub fn preset_for_spec(spec: &InstanceSpec) -> DriverResult<&'static KindPreset> {
    let id = match spec.kind {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Grok => "grok",
        AgentKind::Agy => "agy",
        AgentKind::Generic => spec
            .args
            .iter()
            .find(|arg| preset_by_id(arg).is_some())
            .map(String::as_str)
            .or(spec.model_id.as_deref())
            .unwrap_or("gemini"),
    };
    preset_by_id(id).ok_or_else(|| {
        DriverError::InvalidLaunchSpec(format!("no generic-pty preset for kind {id}"))
    })
}

/// Wait target for [`GenericPtyDriver::wait`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitUntil {
    /// Herdr idle (and `done` when the preset says so).
    Idle,
    /// Herdr `done`.
    Done,
    /// Herdr blocked (permission / prompt).
    Blocked,
}

/// Construction options for [`GenericPtyDriver`].
pub struct GenericPtyOptions {
    /// Provider profile used at materialize time.
    pub profile: ProviderProfile,
    /// Directory for launch overlay files.
    pub launch_dir: PathBuf,
    /// Registered native home.
    pub native_home: PathBuf,
    /// Binary to pin; `Command` uses the preset default.
    pub binary: BinarySource,
    /// Launch origin.
    pub origin: LaunchOrigin,
    /// Isolated Herdr session name.
    pub session_name: String,
    /// Test socket directory (`{dir}/herdr.sock`).
    pub socket_dir: Option<PathBuf>,
    /// Override the `herdr` binary.
    pub herdr_binary: Option<PathBuf>,
    /// Secret resolver.
    pub broker: Arc<dyn SecretBroker>,
    /// Extra env (no secrets logged).
    pub extra_env: BTreeMap<String, String>,
    /// `agent.start` timeout in milliseconds.
    pub agent_start_timeout_ms: u64,
    /// How long to wait after `agent.start` for idle/working/blocked.
    pub liveness_timeout_ms: u64,
    /// Optional screen regex; emits a lifecycle event when a line matches.
    pub line_matcher: Option<String>,
}

impl GenericPtyOptions {
    /// Isolated `remuda-test` session, human origin.
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
            agent_start_timeout_ms: 120_000,
            liveness_timeout_ms: 60_000,
            line_matcher: Some("^DONE".into()),
        }
    }
}

struct PtyLive {
    client: Client,
    pane_id: String,
    agent_name: String,
    recipe: LaunchRecipe,
    events: mpsc::Sender<Observation>,
    status_task: Option<JoinHandle<()>>,
    matcher_task: Option<JoinHandle<()>>,
    closed: bool,
    failed: Arc<AtomicBool>,
}

/// Herdr-hosted generic TUI for any registered kind.
pub struct GenericPtyDriver {
    options: GenericPtyOptions,
    inner: Mutex<Option<PtyLive>>,
    seq: Arc<AtomicU64>,
    closed: AtomicBool,
    ready: watch::Sender<bool>,
    /// Kept so [`Self::ready`] stays open when no waiter has subscribed yet.
    _ready_rx: watch::Receiver<bool>,
}

impl GenericPtyDriver {
    /// Build a driver from explicit options.
    pub fn new(options: GenericPtyOptions) -> Self {
        let (ready, ready_rx) = watch::channel(false);
        Self {
            options,
            inner: Mutex::new(None),
            seq: Arc::new(AtomicU64::new(0)),
            closed: AtomicBool::new(false),
            ready,
            _ready_rx: ready_rx,
        }
    }

    fn mark_ready(&self) {
        let _ = self.ready.send(true);
    }

    fn fail_control(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.ready.send(true);
    }

    async fn live_client(&self) -> DriverResult<(Client, String)> {
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
        if live.failed.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        Ok((live.client.clone(), live.agent_name.clone()))
    }

    /// Send named keys (`enter`, `esc`, `ctrl+c`, …).
    pub async fn send_keys(&self, keys: Vec<String>) -> DriverResult<DriverAck> {
        let (client, agent_name) = self.live_client().await?;
        client
            .agent_send_keys(&agent_name, keys)
            .await
            .map_err(map_herdr)?;
        Ok(DriverAck::transport_written())
    }

    /// Wait until herdr reports idle, done, or blocked.
    pub async fn wait(&self, until: WaitUntil, timeout_ms: u64) -> DriverResult<DriverAck> {
        let (client, agent_name) = self.live_client().await?;
        let mut statuses = match until {
            WaitUntil::Idle => vec![AgentStatus::Idle],
            WaitUntil::Done => vec![AgentStatus::Done],
            WaitUntil::Blocked => vec![AgentStatus::Blocked],
        };
        if matches!(until, WaitUntil::Idle) {
            statuses.push(AgentStatus::Done);
        }
        client
            .agent_wait(AgentWaitParams {
                target: agent_name,
                until: statuses,
                timeout_ms: Some(timeout_ms),
            })
            .await
            .map_err(map_herdr)?;
        Ok(DriverAck::transport_written())
    }

    /// Read recent screen lines via `agent.read` (not the journal).
    pub async fn read_screen(&self, lines: u32) -> DriverResult<String> {
        let (client, agent_name) = self.live_client().await?;
        let read = client
            .agent_read(AgentReadParams {
                target: agent_name,
                source: ReadSource::RecentUnwrapped,
                lines: Some(lines),
                format: ReadFormat::Text,
                strip_ansi: true,
            })
            .await
            .map_err(map_herdr)?;
        Ok(read.text().to_string())
    }

    async fn launch(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        if spec.driver != DriverKind::GenericPty {
            return Err(DriverError::InvalidLaunchSpec(
                "GenericPtyDriver requires driverKind generic-pty".into(),
            ));
        }
        let preset = preset_for_spec(&spec)?;
        let binary = match &self.options.binary {
            BinarySource::Command(name) if name == "claude" && preset.binary != "claude" => {
                BinarySource::Command(preset.binary.to_string())
            }
            other => other.clone(),
        };
        let request = MaterializeRequest {
            spec: &spec,
            profile: &self.options.profile,
            launch_dir: self.options.launch_dir.clone(),
            native_home: self.options.native_home.clone(),
            session: SessionAction::New {
                session_id: uuid::Uuid::now_v7().to_string(),
            },
            launch_id: Id::new("launch")?,
            binary,
            setting_sources: None,
            origin: self.options.origin,
        };
        let mut recipe = materialize(&request)?;
        merge_yolo_argv(&mut recipe.argv, preset);
        let agent_name = agent_name_for(&spec);
        if let Some(flag) = preset.name_flag {
            ensure_name_flag(&mut recipe.argv, flag, &agent_name);
        }
        refuse_bare(&recipe.argv)?;
        if preset.journals {
            recipe = inject_session_start_hook(&mut recipe, &self.options.launch_dir)?;
        }

        let session_name = self.options.session_name.clone();
        if session_name == "default" && self.options.socket_dir.is_none() {
            return Err(DriverError::InvalidLaunchSpec(
                "refusing the default herdr session unless socket_dir is explicit".into(),
            ));
        }
        let server = HerdrServer::ensure(&session_name, self.options.socket_dir.clone())
            .await
            .map_err(map_herdr)?;
        let client = bind_client(&server, &self.options)?;
        let mut env = HashMap::new();
        // Only pin CLAUDE_CONFIG_DIR when the native home already has a login
        // file. An empty isolated dir makes Claude 2.1 report "Not logged in"
        // even when the host user is authenticated.
        let login = std::path::Path::new(&recipe.native_home).join(".claude.json");
        if login.is_file() {
            env.insert("CLAUDE_CONFIG_DIR".into(), recipe.native_home.clone());
        }
        for (key, value) in &self.options.extra_env {
            env.insert(key.clone(), value.clone());
        }
        let created = client
            .workspace_create(WorkspaceCreateParams {
                cwd: Some(recipe.cwd.clone()),
                env: env.clone(),
                focus: false,
                label: Some("remuda".into()),
                source_workspace_id: None,
            })
            .await
            .map_err(map_herdr)?;
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
        wait_shell_prompt(&client, &pane_id).await;
        let started = client
            .agent_start(AgentStartParams {
                name: agent_name.clone(),
                kind: preset.herdr_kind.into(),
                pane_id: pane_id.clone(),
                args: recipe.argv.clone(),
                timeout_ms: Some(self.options.agent_start_timeout_ms),
            })
            .await
            .map_err(|err| {
                tracing::error!(error = %err, kind = preset.id, "generic-pty agent.start failed");
                map_herdr(err)
            })?;
        refuse_bare(&started.argv)?;
        let _ = started;
        dismiss_startup_prompt(&client, &agent_name).await;

        let stream = client
            .subscribe(vec![
                Subscription::pane_agent_status_changed(&pane_id),
                Subscription::pane_exited(),
            ])
            .await
            .map_err(map_herdr)?;
        let (tx, rx) = mpsc::channel(64);
        self.seq.store(0, Ordering::SeqCst);
        self.closed.store(false, Ordering::SeqCst);
        let ctx = obs_ctx(
            DriverKind::GenericPty,
            InstanceId::new(),
            spec.host.clone(),
            Id::new("obj")?,
            RunId::new(),
            agent_name.clone(),
            recipe.binary.version.clone(),
        );
        let failed = Arc::new(AtomicBool::new(false));
        let settled = wait_agent_settled(
            &client,
            &agent_name,
            &pane_id,
            preset.binary,
            &recipe.argv,
            self.options.liveness_timeout_ms,
        )
        .await;
        let session_status = match settled {
            Ok(status) => format!("{status:?}").to_ascii_lowercase(),
            Err(reason) => {
                failed.store(true, Ordering::SeqCst);
                emit_startup_failure(
                    &tx,
                    &self.seq,
                    &ctx,
                    &agent_name,
                    &pane_id,
                    preset.id,
                    &reason,
                )
                .await?;
                let mut ack = DriverAck::transport_written();
                ack.native_ids.insert("paneId".into(), pane_id);
                ack.native_ids.insert("agentName".into(), agent_name);
                ack.native_ids
                    .insert("herdrKind".into(), preset.herdr_kind.into());
                ack.native_ids.insert("lastError".into(), reason);
                info!(kind = preset.id, "generic-pty agent.start failed liveness");
                self.fail_control();
                return Ok(RunHandle::new(recipe, ack, rx));
            }
        };
        emit_on(
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
                        value: agent_name.clone(),
                    },
                    status: Knowledge::Known {
                        value: session_status,
                    },
                    related_ids: BTreeMap::from([
                        ("paneId".into(), pane_id.clone()),
                        ("kind".into(), preset.id.to_string()),
                    ]),
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                },
            )))),
        )
        .await?;
        let status_task = spawn_status_pump(
            stream,
            tx.clone(),
            ctx.clone(),
            Arc::clone(&self.seq),
            pane_id.clone(),
            Arc::clone(&failed),
        );
        let matcher_task = Some(spawn_screen_pump(
            client.clone(),
            agent_name.clone(),
            self.options.line_matcher.clone(),
            tx.clone(),
            ctx.clone(),
            Arc::clone(&self.seq),
        ));
        let mut ack = DriverAck::transport_written();
        ack.native_ids.insert("paneId".into(), pane_id.clone());
        ack.native_ids
            .insert("agentName".into(), agent_name.clone());
        ack.native_ids
            .insert("herdrKind".into(), preset.herdr_kind.into());
        *self.inner.lock().await = Some(PtyLive {
            client,
            pane_id,
            agent_name,
            recipe: recipe.clone(),
            events: tx,
            status_task: Some(status_task),
            matcher_task,
            closed: false,
            failed,
        });
        self.mark_ready();
        info!(kind = preset.id, "generic-pty agent.start dispatched");
        Ok(RunHandle::new(recipe, ack, rx))
    }
}

#[async_trait]
impl Driver for GenericPtyDriver {
    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        let pin = pin_source(&self.options.binary)?;
        Ok(capability_snapshot(
            DriverKind::GenericPty,
            &pin,
            U64(1),
            U64(1),
        )?)
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        match self.launch(spec).await {
            Ok(handle) => Ok(handle),
            Err(error) => {
                self.fail_control();
                Err(error)
            }
        }
    }

    async fn wait_control(&self) -> DriverResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        if !*self.ready.borrow() {
            let mut rx = self.ready.subscribe();
            tokio::time::timeout(CONTROL_READY_TIMEOUT, rx.wait_for(|ready| *ready))
                .await
                .map_err(|_| DriverError::ControlUnavailable)?
                .map_err(|_| DriverError::ControlUnavailable)?;
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        Ok(())
    }

    async fn attach(&self, _native_ref: NativeRef) -> DriverResult<DriverAck> {
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::NativeSessionNotFound)?;
        if live.closed {
            return Err(DriverError::AttachWouldWake);
        }
        let mut ack = DriverAck::not_dispatched();
        ack.native_ids.insert("paneId".into(), live.pane_id.clone());
        Ok(ack)
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        self.wait_control().await?;
        let text = prompt_text(&input)?;
        let (client, agent_name) = self.live_client().await?;
        prompt_when_ready(&client, &agent_name, &text).await?;
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
        let ctx = obs_ctx(
            DriverKind::GenericPty,
            InstanceId::new(),
            HostId::new(),
            Id::new("obj")?,
            RunId::new(),
            live.agent_name.clone(),
            live.recipe.binary.version.clone(),
        );
        let _ = emit_on(
            &live.events,
            &self.seq,
            &ctx,
            SourceChannel::Herdr,
            Completeness::ScreenDerived,
            ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                NativeLifecycle {
                    topic: LifecycleTopic::Turn,
                    native_name: "prompt_echo".into(),
                    native_id: Knowledge::Known {
                        value: live.agent_name.clone(),
                    },
                    status: Knowledge::Known { value: text },
                    related_ids: BTreeMap::new(),
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                },
            )))),
        )
        .await;
        Ok(DriverAck::transport_written())
    }

    async fn send_keys(&self, keys: Vec<String>) -> DriverResult<DriverAck> {
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
        live.client
            .agent_send_keys(&live.agent_name, keys)
            .await
            .map_err(map_herdr)?;
        Ok(DriverAck::transport_written())
    }

    async fn cancel(&self) -> DriverResult<DriverAck> {
        GenericPtyDriver::send_keys(self, vec!["esc".into()]).await
    }

    async fn respond_interaction(
        &self,
        _id: InteractionId,
        _answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        Err(DriverError::CapabilityUnsupported(
            "generic-pty has no structured interaction channel".into(),
        ))
    }

    async fn close(&self) -> DriverResult<DriverAck> {
        self.fail_control();
        let mut inner = self.inner.lock().await;
        let Some(live) = inner.as_mut() else {
            return Ok(DriverAck::not_dispatched());
        };
        live.closed = true;
        if let Some(task) = live.status_task.take() {
            task.abort();
        }
        if let Some(task) = live.matcher_task.take() {
            task.abort();
        }
        let pane_id = live.pane_id.clone();
        let client = live.client.clone();
        drop(inner);
        let _ = client.pane_close(pane_id).await;
        Ok(DriverAck::not_dispatched())
    }

    async fn resume(&self, _native_ref: NativeRef) -> DriverResult<RunHandle> {
        Err(DriverError::CapabilityUnsupported(
            "generic-pty has no semantic resume".into(),
        ))
    }
}

fn spawn_status_pump(
    mut stream: EventStream,
    tx: mpsc::Sender<Observation>,
    ctx: crate::claude_pty::ObsCtx,
    seq: Arc<AtomicU64>,
    pane_id: String,
    failed: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(item) = stream.next_event().await {
            let Ok(event) = item else {
                break;
            };
            if event.kind == EventKind::PaneExited {
                if event.pane_id() != Some(pane_id.as_str()) {
                    continue;
                }
                failed.store(true, Ordering::SeqCst);
                let payload = failure_lifecycle(
                    "exit",
                    &ctx.session_id,
                    &pane_id,
                    "pane exited; agent process is gone",
                );
                if emit_on(
                    &tx,
                    &seq,
                    &ctx,
                    SourceChannel::Herdr,
                    Completeness::ScreenDerived,
                    payload,
                )
                .await
                .is_err()
                {
                    break;
                }
                continue;
            }
            if event.kind != EventKind::PaneAgentStatusChanged {
                continue;
            }
            let Some(status) = event.agent_status() else {
                continue;
            };
            let label = match status {
                AgentStatus::Working => "working",
                AgentStatus::Idle => "idle",
                AgentStatus::Blocked => "blocked",
                AgentStatus::Done => "done",
                AgentStatus::Unknown => "unknown",
            };
            let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(
                Box::new(NativeLifecycle {
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
                }),
            )));
            if emit_on(
                &tx,
                &seq,
                &ctx,
                SourceChannel::Herdr,
                Completeness::ScreenDerived,
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

fn spawn_screen_pump(
    client: Client,
    agent_name: String,
    pattern: Option<String>,
    tx: mpsc::Sender<Observation>,
    ctx: crate::claude_pty::ObsCtx,
    seq: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut seen_match = false;
        let mut last_snapshot: Option<String> = None;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Ok(read) = client
                .agent_read(AgentReadParams {
                    target: agent_name.clone(),
                    source: ReadSource::RecentUnwrapped,
                    lines: Some(SCREEN_SNAPSHOT_LINES as u32),
                    format: ReadFormat::Text,
                    strip_ansi: true,
                })
                .await
            else {
                continue;
            };
            let text = last_n_lines(read.text(), SCREEN_SNAPSHOT_LINES);
            if last_snapshot.as_deref() != Some(text.as_str()) {
                last_snapshot = Some(text.clone());
                let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(
                    Box::new(NativeLifecycle {
                        topic: LifecycleTopic::Turn,
                        native_name: "screen".into(),
                        native_id: Knowledge::Known {
                            value: agent_name.clone(),
                        },
                        status: Knowledge::Known {
                            value: text.clone(),
                        },
                        related_ids: BTreeMap::from([(
                            "lines".into(),
                            SCREEN_SNAPSHOT_LINES.to_string(),
                        )]),
                        data_ref: None,
                        severity: Severity::Info,
                        affects_completion: false,
                    }),
                )));
                if emit_on(
                    &tx,
                    &seq,
                    &ctx,
                    SourceChannel::Pty,
                    Completeness::ScreenDerived,
                    payload,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            if let Some(pattern) = &pattern
                && !seen_match
                && line_matches(&text, pattern)
            {
                seen_match = true;
                let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(
                    Box::new(NativeLifecycle {
                        topic: LifecycleTopic::Turn,
                        native_name: "line-matcher".into(),
                        native_id: Knowledge::Known {
                            value: agent_name.clone(),
                        },
                        status: Knowledge::Known {
                            value: "matched".into(),
                        },
                        related_ids: BTreeMap::from([("pattern".into(), pattern.clone())]),
                        data_ref: None,
                        severity: Severity::Info,
                        affects_completion: true,
                    }),
                )));
                if emit_on(
                    &tx,
                    &seq,
                    &ctx,
                    SourceChannel::Herdr,
                    Completeness::ScreenDerived,
                    payload,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        }
    })
}

fn last_n_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

async fn prompt_when_ready(client: &Client, agent_name: &str, text: &str) -> DriverResult<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match client
            .agent_prompt(AgentPromptParams {
                target: agent_name.to_owned(),
                text: text.to_owned(),
                wait: None,
            })
            .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                let mapped = map_prompt_herdr(err);
                if !matches!(mapped, DriverError::ControlUnavailable) || Instant::now() >= deadline
                {
                    return Err(mapped);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Prefix / substring matcher used for `^DONE ` without a regex crate.
pub fn line_matches(screen: &str, pattern: &str) -> bool {
    let anchored_end = pattern.ends_with('$');
    let body = pattern
        .strip_prefix('^')
        .unwrap_or(pattern)
        .strip_suffix('$')
        .unwrap_or(pattern.strip_prefix('^').unwrap_or(pattern));
    let body = if anchored_end {
        pattern
            .strip_prefix('^')
            .unwrap_or(pattern)
            .trim_end_matches('$')
    } else {
        body
    };
    let prefix = pattern.starts_with('^');
    screen.lines().any(|line| {
        if prefix && anchored_end {
            line == body
        } else if prefix {
            line.starts_with(body)
        } else if anchored_end {
            line.ends_with(body)
        } else {
            line.contains(body)
        }
    })
}

async fn wait_shell_prompt(client: &Client, pane_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(read) = client
            .pane_read(PaneReadParams {
                pane_id: pane_id.to_owned(),
                source: ReadSource::RecentUnwrapped,
                lines: Some(20),
                format: ReadFormat::Text,
                strip_ansi: true,
            })
            .await
        {
            let text = read.text();
            if text.contains('➜') || text.contains('$') || text.contains('%') || text.contains('>')
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn dismiss_startup_prompt(client: &Client, agent_name: &str) {
    tokio::time::sleep(Duration::from_millis(400)).await;
    let Ok(read) = client
        .agent_read(AgentReadParams {
            target: agent_name.to_owned(),
            source: ReadSource::RecentUnwrapped,
            lines: Some(40),
            format: ReadFormat::Text,
            strip_ansi: true,
        })
        .await
    else {
        return;
    };
    let text = read.text();
    if text.contains("Do you trust")
        || text.contains("Yes, continue")
        || text.contains("Trust this")
    {
        let _ = client
            .agent_send_keys(agent_name, vec!["enter".into()])
            .await;
    }
}

async fn emit_startup_failure(
    tx: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &crate::claude_pty::ObsCtx,
    agent_name: &str,
    pane_id: &str,
    kind: &str,
    reason: &str,
) -> DriverResult<()> {
    let mut payload = failure_lifecycle("error", agent_name, pane_id, reason);
    if let ObservationPayload::Lifecycle(body) = &mut payload
        && let LifecyclePayload::Native(native) = body.as_mut()
    {
        native.related_ids.insert("kind".into(), kind.to_string());
    }
    emit_on(
        tx,
        seq,
        ctx,
        SourceChannel::Herdr,
        Completeness::Structured,
        payload,
    )
    .await
}

fn failure_lifecycle(
    native_name: &str,
    agent_name: &str,
    pane_id: &str,
    reason: &str,
) -> ObservationPayload {
    ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
        NativeLifecycle {
            topic: LifecycleTopic::Session,
            native_name: native_name.into(),
            native_id: Knowledge::Known {
                value: agent_name.into(),
            },
            status: Knowledge::Known {
                value: reason.into(),
            },
            related_ids: BTreeMap::from([
                ("paneId".into(), pane_id.into()),
                ("lastError".into(), reason.into()),
            ]),
            data_ref: None,
            severity: Severity::Error,
            affects_completion: true,
        },
    ))))
}

enum Liveness {
    Settled(AgentStatus),
    Crash(String),
    Pending {
        gone: Option<String>,
        shell: Option<String>,
    },
}

async fn wait_agent_settled(
    client: &Client,
    agent_name: &str,
    pane_id: &str,
    binary: &str,
    argv: &[String],
    timeout_ms: u64,
) -> Result<AgentStatus, String> {
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_gone = None;
    let mut last_shell = None;
    loop {
        match probe_liveness(client, agent_name, pane_id, binary, argv).await {
            Liveness::Settled(status) => return Ok(status),
            Liveness::Crash(reason) => return Err(reason),
            Liveness::Pending { gone, shell } => {
                if gone.is_some() {
                    last_gone = gone;
                }
                if shell.is_some() {
                    last_shell = shell;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(last_gone.or(last_shell).unwrap_or_else(|| {
                format!(
                    "agent {agent_name} did not become idle/working/blocked within {}ms",
                    timeout.as_millis()
                )
            }));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn probe_liveness(
    client: &Client,
    agent_name: &str,
    pane_id: &str,
    binary: &str,
    argv: &[String],
) -> Liveness {
    let screen = match client
        .agent_read(AgentReadParams {
            target: agent_name.to_string(),
            source: ReadSource::RecentUnwrapped,
            lines: Some(40),
            format: ReadFormat::Text,
            strip_ansi: true,
        })
        .await
    {
        Ok(read) => Some(read.text().to_string()),
        Err(_) => client
            .pane_read(PaneReadParams {
                pane_id: pane_id.to_owned(),
                source: ReadSource::RecentUnwrapped,
                lines: Some(40),
                format: ReadFormat::Text,
                strip_ansi: true,
            })
            .await
            .ok()
            .map(|read| read.text().to_string()),
    };
    if let Some(text) = screen.as_deref()
        && looks_like_cli_crash(text)
    {
        return Liveness::Crash(cli_crash_reason(text));
    }

    let shell_only = client
        .pane_process_info(Some(pane_id.to_string()))
        .await
        .ok()
        .and_then(|info| info.process_info)
        .is_some_and(|process| process_is_shell_only(&process, binary, argv));
    let at_shell = screen.as_deref().is_some_and(looks_like_shell_prompt);
    let shell = (shell_only && at_shell).then(|| {
        format!("agent process exited during startup; pane {pane_id} returned to a shell prompt")
    });

    match client.agent_get(agent_name).await {
        Ok(info) => match info.agent.agent_status {
            AgentStatus::Idle | AgentStatus::Working | AgentStatus::Blocked => {
                Liveness::Settled(info.agent.agent_status)
            }
            AgentStatus::Done => Liveness::Settled(AgentStatus::Idle),
            AgentStatus::Unknown => Liveness::Pending { gone: None, shell },
        },
        Err(remuda_herdr::Error::Api { code, message, .. })
            if code == "agent_not_ready" || message.contains("unknown agent") =>
        {
            Liveness::Pending {
                gone: Some(format!("agent gone / {code}: {message}")),
                shell,
            }
        }
        Err(_) => Liveness::Pending { gone: None, shell },
    }
}

fn process_is_shell_only(
    process: &remuda_herdr::PaneProcessInfo,
    binary: &str,
    argv: &[String],
) -> bool {
    if process
        .foreground_processes
        .iter()
        .any(|proc| process_looks_like_agent(proc, binary, argv))
    {
        return false;
    }
    process.foreground_processes.is_empty()
        || process.foreground_processes.iter().all(|proc| {
            is_shell_name(&proc.name) || proc.argv0.as_deref().is_some_and(is_shell_name)
        })
}

fn process_looks_like_agent(
    proc: &remuda_herdr::PaneProcessInfoProcess,
    binary: &str,
    argv: &[String],
) -> bool {
    let names = [proc.name.as_str(), proc.argv0.as_deref().unwrap_or("")];
    if names
        .iter()
        .any(|name| !name.is_empty() && name.rsplit('/').next() == Some(binary))
    {
        return true;
    }
    argv.iter().any(|token| {
        PathBuf::from(token).is_file()
            && proc
                .argv
                .as_ref()
                .is_some_and(|cmd| cmd.iter().any(|part| part == token))
    })
}

fn is_shell_name(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    let base = base.trim_start_matches('-');
    matches!(
        base,
        "zsh" | "bash" | "sh" | "fish" | "dash" | "ksh" | "tcsh" | "nu"
    )
}

/// Last non-empty line is a typical login-shell prompt (`%`, `$`, `#`).
pub fn looks_like_shell_prompt(screen: &str) -> bool {
    let last = screen
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    last == "%"
        || last == "$"
        || last == "#"
        || last.ends_with(" %")
        || last.ends_with(" $")
        || last.ends_with(" #")
}

fn looks_like_cli_crash(screen: &str) -> bool {
    let lower = screen.to_ascii_lowercase();
    lower.contains("unexpected argument")
        || lower.contains("unknown option")
        || lower.contains("unrecognized option")
        || (lower.contains("error:") && looks_like_shell_prompt(screen))
}

fn cli_crash_reason(screen: &str) -> String {
    screen
        .lines()
        .map(str::trim)
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("unexpected argument")
                || lower.contains("unknown option")
                || lower.contains("error:")
        })
        .unwrap_or("agent process exited during startup")
        .to_string()
}

fn map_prompt_herdr(error: remuda_herdr::Error) -> DriverError {
    match &error {
        remuda_herdr::Error::Api { code, message, .. }
            if code == "agent_not_ready" || message.contains("unknown agent") =>
        {
            DriverError::ControlUnavailable
        }
        _ => map_herdr(error),
    }
}

fn merge_yolo_argv(argv: &mut Vec<String>, preset: &KindPreset) {
    for flag in preset.yolo_argv {
        if !argv.iter().any(|token| token == flag) {
            argv.push((*flag).to_string());
        }
    }
}

fn ensure_name_flag(argv: &mut Vec<String>, flag: &str, name: &str) {
    if argv.windows(2).any(|pair| pair[0] == flag) {
        return;
    }
    argv.push(flag.into());
    argv.push(name.into());
}

fn agent_name_for(spec: &InstanceSpec) -> String {
    let kind = match spec.kind {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Grok => "grok",
        AgentKind::Agy => "agy",
        AgentKind::Generic => "pty",
    };
    let uniq: String = uuid::Uuid::now_v7()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect();
    format!("rmd-{kind}-{uniq}")
}

fn bind_client(server: &HerdrServer, options: &GenericPtyOptions) -> DriverResult<Client> {
    let mut client = Client::connect(server.socket_path()).with_timeout(Duration::from_secs(30));
    if options.socket_dir.is_none() {
        client = client.with_session_name(server.session_name());
    }
    if let Some(binary) = &options.herdr_binary {
        client = client.with_binary(binary);
    }
    let _ = dummy_digest()?;
    let _ = HerdrPin {
        binary_path: client.binary().to_string_lossy().into_owned(),
        version: "runtime".into(),
        digest: dummy_digest()?,
        protocol_version: "22".into(),
        server_identity: Id::new("obj")?,
        server_epoch: Id::new("epoch")?,
        representation: HerdrRepresentation::RenderedAnsi,
    };
    Ok(client)
}

fn pin_source(source: &BinarySource) -> DriverResult<BinaryPin> {
    match source {
        BinarySource::Pinned(pin) => Ok(pin.clone()),
        BinarySource::Path(path) => pin_binary(path),
        BinarySource::Command(name) => pin_binary(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_cover_dogfood_kinds() {
        for id in ["claude", "codex", "grok", "agy", "gemini"] {
            assert!(preset_by_id(id).is_some(), "{id}");
        }
        assert!(preset_by_id("codex").unwrap().yolo_argv[0].contains("bypass"));
        assert!(preset_by_id("grok").unwrap().name_flag.is_none());
        assert!(preset_by_id("codex").unwrap().name_flag.is_none());
        assert!(preset_by_id("agy").unwrap().name_flag.is_none());
        assert!(preset_by_id("claude").unwrap().journals);
        assert!(!preset_by_id("codex").unwrap().journals);
    }

    #[test]
    fn done_line_matcher() {
        assert!(line_matches("hello\nDONE abc\n", "^DONE "));
        assert!(!line_matches("not yet\n", "^DONE "));
        assert!(line_matches("DONE sha", "^DONE "));
        assert!(line_matches("DONE\n", "^DONE"));
        assert!(line_matches("hello\nDONE\n", "^DONE"));
    }

    #[test]
    fn shell_prompt_detects_zsh_percent_not_fake_herdr_idle() {
        assert!(looks_like_shell_prompt(
            "error: unexpected argument '--name' found\n%\n"
        ));
        assert!(!looks_like_shell_prompt("❯ \n"));
        assert!(!looks_like_shell_prompt("OK\n❯ \n"));
    }
}
