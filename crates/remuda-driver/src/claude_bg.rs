//! `claude --bg` driver (plan M0-10).
//!
//! First prompt is deferred argv (explicit non-secret opt-in). `--session-id` is
//! stripped. Observe `jobs/<id>/state.json` plus jsonl when a SessionStart hook
//! records `transcript_path`. `attach` is read-only and must not wake a stopped
//! job. `open_terminal` is the only path that runs `claude attach` in a Herdr
//! pane. `stop` is `claude stop`, never `rm`.

use crate::binary::{BinaryPin, pin_binary};
use crate::capabilities::capability_snapshot;
use crate::claude_pty::{
    ObsCtx, argv_for_resume, emit_on, map_herdr, obs_ctx, prompt_text, refuse_bare,
    strip_named_flags,
};
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::materializer::{
    BinarySource, LaunchOrigin, MaterializeRequest, SessionAction, materialize,
};
use crate::profile::{EnvFileSecretBroker, ProviderProfile, SecretBroker};
use crate::recipe::LaunchRecipe;
use async_trait::async_trait;
use remuda_herdr::{Client, HerdrServer, PaneSplitParams, SplitDirection, WorkspaceCreateParams};
use remuda_protocol::{
    AgentKind, Completeness, DriverInput, DriverKind, HostId, Id, InstanceId, InstanceSpec,
    InteractionAnswer, InteractionId, Knowledge, LifecyclePayload, LifecycleTopic, NativeLifecycle,
    NativeRef, Observation, ObservationPayload, RunId, Severity, SourceChannel, U64,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Construction options for [`ClaudeBgDriver`].
pub struct ClaudeBgOptions {
    /// Provider profile used at materialize time.
    pub profile: ProviderProfile,
    /// Directory for 0700 launch files.
    pub launch_dir: PathBuf,
    /// Registered `CLAUDE_CONFIG_DIR` (jobs/ and transcripts live here when set).
    pub native_home: PathBuf,
    /// `claude` binary to pin and exec.
    pub binary: BinarySource,
    /// Launch origin; bot/dispatcher cannot request bypass (D-011).
    pub origin: LaunchOrigin,
    /// Isolated Herdr session used only by [`ClaudeBgDriver::open_terminal`].
    pub session_name: String,
    /// If set, API socket is `{socket_dir}/herdr.sock`.
    pub socket_dir: Option<PathBuf>,
    /// Override the `herdr` binary for explicit attach panes.
    pub herdr_binary: Option<PathBuf>,
    /// Secret resolver. Defaults to [`EnvFileSecretBroker`].
    pub broker: Arc<dyn SecretBroker>,
    /// Extra env for the `--bg` process (no secrets logged).
    pub extra_env: BTreeMap<String, String>,
    /// Override `--setting-sources`.
    pub setting_sources: Option<Vec<String>>,
}

impl ClaudeBgOptions {
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
        }
    }
}

struct BgLive {
    short_id: String,
    name: String,
    session_id: Option<String>,
    recipe: LaunchRecipe,
    instance_id: InstanceId,
    host_id: HostId,
    journal_id: Id,
    run_id: RunId,
    #[allow(dead_code)]
    native_store_id: Id,
    events: mpsc::Sender<Observation>,
    observer: Option<JoinHandle<()>>,
    stopped: bool,
    dispatched: bool,
    attach_pane: Option<String>,
}

/// Native `claude --bg` job driver.
pub struct ClaudeBgDriver {
    options: ClaudeBgOptions,
    inner: Mutex<Option<BgLive>>,
    last_recipe: Mutex<Option<LaunchRecipe>>,
    closed: AtomicBool,
    seq: Arc<AtomicU64>,
}

impl ClaudeBgDriver {
    /// Build a driver from explicit options.
    pub fn new(options: ClaudeBgOptions) -> Self {
        Self {
            options,
            inner: Mutex::new(None),
            last_recipe: Mutex::new(None),
            closed: AtomicBool::new(false),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Last persisted recipe, if any.
    pub async fn persisted_recipe(&self) -> Option<LaunchRecipe> {
        self.last_recipe.lock().await.clone()
    }

    /// Explicit user action: run `claude attach <shortId>` in a Herdr pane.
    ///
    /// Read-only `Driver::attach` must not call this. Browser refresh and Node
    /// reconciliation must not call this.
    pub async fn open_terminal(&self) -> DriverResult<DriverAck> {
        let mut inner = self.inner.lock().await;
        let live = inner.as_mut().ok_or(DriverError::ControlUnavailable)?;
        if live.stopped || !live.dispatched {
            return Err(DriverError::AttachWouldWake);
        }
        let short_id = live.short_id.clone();
        let cwd = live.recipe.cwd.clone();
        let binary = live.recipe.binary.abs_path.clone();
        let server =
            HerdrServer::ensure(&self.options.session_name, self.options.socket_dir.clone())
                .await
                .map_err(map_herdr)?;
        let mut client =
            Client::connect(server.socket_path()).with_timeout(Duration::from_secs(30));
        if self.options.socket_dir.is_none() {
            client = client.with_session_name(server.session_name());
        }
        if let Some(herdr) = &self.options.herdr_binary {
            client = client.with_binary(herdr);
        }
        let created = client
            .workspace_create(WorkspaceCreateParams {
                cwd: Some(cwd.clone()),
                label: Some("remuda-bg-attach".into()),
                focus: false,
                ..WorkspaceCreateParams::default()
            })
            .await
            .map_err(map_herdr)?;
        let split = client
            .pane_split(PaneSplitParams {
                direction: SplitDirection::Right,
                target_pane_id: Some(created.root_pane.pane_id.clone()),
                cwd: Some(cwd),
                focus: false,
                workspace_id: None,
                ratio: None,
                env: Default::default(),
            })
            .await
            .map_err(map_herdr)?;
        let pane_id = split.pane.pane_id.clone();
        let command = format!(
            "{} attach {}\n",
            shell_single_quote(&binary),
            shell_single_quote(&short_id)
        );
        client
            .pane_send_text(&pane_id, command)
            .await
            .map_err(map_herdr)?;
        live.attach_pane = Some(pane_id.clone());
        let mut ack = DriverAck::transport_written();
        ack.native_ids.insert("jobId".into(), short_id);
        ack.native_ids.insert("paneId".into(), pane_id);
        ack.native_ids
            .insert("herdrSession".into(), self.options.session_name.clone());
        Ok(ack)
    }

    async fn prepare(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        if spec.driver != DriverKind::ClaudeBg {
            return Err(DriverError::InvalidLaunchSpec(
                "ClaudeBgDriver requires driverKind claude-bg".into(),
            ));
        }
        if self.options.origin == LaunchOrigin::Bot
            && let remuda_protocol::PermissionMode::Claude(claude) = &spec.permission_mode
            && matches!(
                claude.mode,
                remuda_protocol::ClaudePermissionMode::BypassPermissions
                    | remuda_protocol::ClaudePermissionMode::DontAsk
            )
        {
            return Err(DriverError::BypassNotAllowedForBot);
        }
        let request = MaterializeRequest {
            spec: &spec,
            profile: &self.options.profile,
            launch_dir: self.options.launch_dir.clone(),
            native_home: self.options.native_home.clone(),
            session: SessionAction::New {
                session_id: uuid::Uuid::now_v7().to_string(),
            },
            launch_id: Id::new("launch")?,
            binary: self.options.binary.clone(),
            setting_sources: self.options.setting_sources.clone(),
            origin: self.options.origin,
        };
        let mut recipe = materialize(&request)?;
        recipe.argv = strip_named_flags(&recipe.argv, &["--session-id", "--cwd", "--continue"]);
        if !recipe.argv.iter().any(|token| token == "--bg") {
            recipe.argv.insert(0, "--bg".into());
        }
        refuse_bare(&recipe.argv)?;
        *self.last_recipe.lock().await = Some(recipe.clone());

        let name = bg_instance_name(&spec);
        let (tx, rx) = mpsc::channel(64);
        self.seq.store(0, Ordering::SeqCst);
        self.closed.store(false, Ordering::SeqCst);
        let instance_id = InstanceId::new();
        let run_id = RunId::new();
        let journal_id = Id::new("obj")?;
        *self.inner.lock().await = Some(BgLive {
            short_id: String::new(),
            name,
            session_id: None,
            recipe: recipe.clone(),
            instance_id,
            host_id: spec.host.clone(),
            journal_id,
            run_id,
            native_store_id: spec.native_home.store_id.clone(),
            events: tx,
            observer: None,
            stopped: false,
            dispatched: false,
            attach_pane: None,
        });
        Ok(RunHandle::new(recipe, DriverAck::not_dispatched(), rx))
    }

    async fn dispatch_first(&self, prompt: &str) -> DriverResult<DriverAck> {
        if prompt
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
        {
            return Err(DriverError::InvalidLaunchSpec(
                "bg deferred argv prompt must be a single non-secret text block".into(),
            ));
        }
        let mut inner = self.inner.lock().await;
        let live = inner.as_mut().ok_or(DriverError::ControlUnavailable)?;
        if live.stopped {
            return Err(DriverError::ControlUnavailable);
        }
        if live.dispatched {
            return self.send_followup_locked(live, prompt).await;
        }
        let mut argv = live.recipe.argv.clone();
        ensure_name_flag(&mut argv, &live.name);
        argv.push(prompt.to_string());
        refuse_bare(&argv)?;

        let mut command = Command::new(&live.recipe.binary.abs_path);
        command
            .args(&argv)
            .current_dir(&live.recipe.cwd)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("CLAUDE_CONFIG_DIR", &live.recipe.native_home);
        for (key, value) in &self.options.extra_env {
            command.env(key, value);
        }
        let output = command.output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "claude --bg exited {}: {stderr}{stdout}",
                output.status
            )));
        }
        let short_id = parse_backgrounded(&stdout).ok_or_else(|| {
            DriverError::InvalidLaunchSpec(format!(
                "claude --bg stdout missing backgrounded id: {stdout}"
            ))
        })?;
        live.short_id = short_id.clone();
        live.dispatched = true;
        if let Some(session) = lookup_session_id(
            Path::new(&live.recipe.binary.abs_path),
            &live.recipe.cwd,
            &live.recipe.native_home,
            &live.name,
            &short_id,
        )
        .await
        {
            live.session_id = Some(session);
        }

        let ctx = self.ctx_locked(live);
        let mut related = BTreeMap::new();
        related.insert("jobId".into(), short_id.clone());
        related.insert("name".into(), live.name.clone());
        emit_on(
            &live.events,
            &self.seq,
            &ctx,
            SourceChannel::Runtime,
            Completeness::Structured,
            ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                NativeLifecycle {
                    topic: LifecycleTopic::Session,
                    native_name: "claude-bg".into(),
                    native_id: Knowledge::Known {
                        value: live.session_id.clone().unwrap_or_else(|| short_id.clone()),
                    },
                    status: Knowledge::Known {
                        value: "backgrounded".into(),
                    },
                    related_ids: related,
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                },
            )))),
        )
        .await?;

        let jobs = job_dir(&live.recipe.native_home, &short_id);
        live.observer = Some(spawn_job_observer(
            jobs,
            live.events.clone(),
            ctx,
            Arc::clone(&self.seq),
        ));

        let mut ack = DriverAck::transport_written();
        ack.native_ids.insert("jobId".into(), short_id);
        if let Some(session) = &live.session_id {
            ack.native_ids.insert("sessionId".into(), session.clone());
        }
        ack.native_ids.insert("name".into(), live.name.clone());
        info!(name = %live.name, "claude --bg dispatched");
        Ok(ack)
    }

    async fn send_followup_locked(
        &self,
        live: &mut BgLive,
        prompt: &str,
    ) -> DriverResult<DriverAck> {
        if live.stopped {
            return Err(DriverError::AttachWouldWake);
        }
        let mut argv = strip_named_flags(&live.recipe.argv, &["--session-id", "--cwd"]);
        if !argv.iter().any(|token| token == "--bg") {
            argv.insert(0, "--bg".into());
        }
        argv = argv_for_resume(&argv, &live.short_id);
        argv.push(prompt.to_string());
        refuse_bare(&argv)?;
        let mut command = Command::new(&live.recipe.binary.abs_path);
        command
            .args(&argv)
            .current_dir(&live.recipe.cwd)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("CLAUDE_CONFIG_DIR", &live.recipe.native_home);
        let output = command.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DriverError::InvalidLaunchSpec(format!(
                "claude --bg --resume failed: {stderr}"
            )));
        }
        let mut ack = DriverAck::transport_written();
        ack.native_ids.insert("jobId".into(), live.short_id.clone());
        Ok(ack)
    }

    async fn stop_job(&self) -> DriverResult<DriverAck> {
        let mut inner = self.inner.lock().await;
        let live = inner.as_mut().ok_or(DriverError::ControlUnavailable)?;
        if let Some(task) = live.observer.take() {
            task.abort();
        }
        if live.dispatched && !live.short_id.is_empty() {
            let mut command = Command::new(&live.recipe.binary.abs_path);
            command
                .arg("stop")
                .arg(&live.short_id)
                .current_dir(&live.recipe.cwd)
                .kill_on_drop(true)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env("CLAUDE_CONFIG_DIR", &live.recipe.native_home);
            let status = command.status().await?;
            if !status.success() {
                warn!(job = %live.short_id, %status, "claude stop returned non-zero");
            }
        }
        live.stopped = true;
        let mut ack = DriverAck::not_dispatched();
        if !live.short_id.is_empty() {
            ack.native_ids.insert("jobId".into(), live.short_id.clone());
        }
        Ok(ack)
    }

    fn ctx_locked(&self, live: &BgLive) -> ObsCtx {
        obs_ctx(
            DriverKind::ClaudeBg,
            live.instance_id.clone(),
            live.host_id.clone(),
            live.journal_id.clone(),
            live.run_id.clone(),
            live.session_id
                .clone()
                .unwrap_or_else(|| live.short_id.clone()),
            live.recipe.binary.version.clone(),
        )
    }
}

#[async_trait]
impl Driver for ClaudeBgDriver {
    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        let pin = pin_source(&self.options.binary)?;
        Ok(capability_snapshot(
            DriverKind::ClaudeBg,
            &pin,
            U64(1),
            U64(1),
        )?)
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        self.prepare(spec).await
    }

    async fn attach(&self, native_ref: NativeRef) -> DriverResult<DriverAck> {
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::NativeSessionNotFound)?;
        if live.stopped || !live.dispatched {
            return Err(DriverError::AttachWouldWake);
        }
        if let Some(bg) = &native_ref.claude_bg
            && !bg.job_id.is_empty()
            && bg.job_id != live.short_id
        {
            return Err(DriverError::NativeSessionNotFound);
        }
        if job_is_stopped(&live.recipe.native_home, &live.short_id) {
            return Err(DriverError::AttachWouldWake);
        }
        let mut ack = DriverAck::not_dispatched();
        ack.native_ids.insert("jobId".into(), live.short_id.clone());
        Ok(ack)
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        let text = prompt_text(&input)?;
        self.dispatch_first(&text).await
    }

    async fn cancel(&self) -> DriverResult<DriverAck> {
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
        if live.attach_pane.is_some() {
            drop(inner);
            return Err(DriverError::CapabilityUnknown(
                "cancel on an open bg attach pane is native TTY only".into(),
            ));
        }
        Err(DriverError::CapabilityUnsupported(
            "claude-bg cancel without an attach pane; use close()/claude stop".into(),
        ))
    }

    async fn respond_interaction(
        &self,
        _id: InteractionId,
        _answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        Err(DriverError::CapabilityUnsupported(
            "claude-bg has no structured interaction channel".into(),
        ))
    }

    async fn close(&self) -> DriverResult<DriverAck> {
        self.closed.store(true, Ordering::SeqCst);
        self.stop_job().await
    }

    async fn resume(&self, native_ref: NativeRef) -> DriverResult<RunHandle> {
        let job_id = native_ref
            .claude_bg
            .as_ref()
            .map(|bg| bg.job_id.clone())
            .filter(|id| !id.is_empty())
            .ok_or(DriverError::NativeSessionNotFound)?;
        if job_is_stopped(&self.options.native_home, &job_id) {
            return Err(DriverError::AttachWouldWake);
        }
        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json"))?;
        spec.driver = DriverKind::ClaudeBg;
        spec.kind = AgentKind::Claude;
        spec.host = native_ref.host_id.clone();
        spec.cwd = self
            .last_recipe
            .lock()
            .await
            .as_ref()
            .map(|recipe| recipe.cwd.clone())
            .unwrap_or_else(|| spec.cwd.clone());
        let handle = self.prepare(spec).await?;
        if let Some(live) = self.inner.lock().await.as_mut() {
            live.short_id = job_id;
            live.dispatched = true;
            live.session_id = match native_ref.session_id {
                Knowledge::Known { value } => Some(value),
                _ => native_ref.claude.map(|claude| claude.session_id),
            };
        }
        Ok(handle)
    }
}

/// Parse `backgrounded · <shortId>` from `claude --bg` stdout.
pub fn parse_backgrounded(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix("backgrounded")?;
        let rest = rest.trim_start_matches([' ', '·', '•', '-', ':']);
        let rest = rest.trim_start();
        let id = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches('·')
            .trim();
        if id.len() >= 6 && id.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Some(id.to_string());
        }
    }
    None
}

fn spawn_job_observer(
    jobs: PathBuf,
    tx: mpsc::Sender<Observation>,
    ctx: ObsCtx,
    seq: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let state_path = jobs.join("state.json");
        let mut last = String::new();
        loop {
            if let Ok(body) = tokio::fs::read_to_string(&state_path).await
                && body != last
            {
                last = body.clone();
                if let Ok(value) = serde_json::from_str::<Value>(&body) {
                    let status = value
                        .get("state")
                        .or_else(|| value.get("status"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let session = value
                        .get("sessionId")
                        .or_else(|| value.get("session_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let mut related = BTreeMap::new();
                    if !session.is_empty() {
                        related.insert("sessionId".into(), session.to_string());
                    }
                    let payload = ObservationPayload::Lifecycle(Box::new(
                        LifecyclePayload::Native(Box::new(NativeLifecycle {
                            topic: LifecycleTopic::Turn,
                            native_name: "job_state".into(),
                            native_id: Knowledge::Known {
                                value: if session.is_empty() {
                                    ctx.session_id.clone()
                                } else {
                                    session.to_string()
                                },
                            },
                            status: Knowledge::Known {
                                value: status.to_string(),
                            },
                            related_ids: related,
                            data_ref: None,
                            severity: Severity::Info,
                            affects_completion: false,
                        })),
                    ));
                    if emit_on(
                        &tx,
                        &seq,
                        &ctx,
                        SourceChannel::Runtime,
                        Completeness::Partial,
                        payload,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    })
}

async fn lookup_session_id(
    binary: &Path,
    cwd: &str,
    native_home: &str,
    name: &str,
    short_id: &str,
) -> Option<String> {
    let mut command = Command::new(binary);
    command
        .arg("agents")
        .arg("--json")
        .arg("--cwd")
        .arg(cwd)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CLAUDE_CONFIG_DIR", native_home);
    let output = command.output().await.ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let rows = value.as_array()?;
    for row in rows {
        let id = row.get("id").and_then(Value::as_str).unwrap_or("");
        let row_name = row.get("name").and_then(Value::as_str).unwrap_or("");
        if (id == short_id || row_name == name)
            && let Some(session) = row.get("sessionId").and_then(Value::as_str)
        {
            return Some(session.to_string());
        }
    }
    None
}

fn job_dir(native_home: impl AsRef<Path>, short_id: &str) -> PathBuf {
    native_home.as_ref().join("jobs").join(short_id)
}

fn job_is_stopped(native_home: impl AsRef<Path>, short_id: &str) -> bool {
    let path = job_dir(native_home, short_id).join("state.json");
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&body) else {
        return false;
    };
    matches!(
        value.get("state").and_then(Value::as_str),
        Some("stopped" | "failed" | "crashed" | "exited")
    )
}

fn ensure_name_flag(argv: &mut Vec<String>, name: &str) {
    if argv.windows(2).any(|pair| pair[0] == "--name")
        || argv.iter().any(|token| token.starts_with("--name="))
    {
        return;
    }
    argv.push("--name".into());
    argv.push(name.to_string());
}

fn bg_instance_name(spec: &InstanceSpec) -> String {
    let raw = spec.host.as_id().as_str().to_string();
    let suffix = raw
        .rsplit('_')
        .next()
        .unwrap_or("bg")
        .chars()
        .take(8)
        .collect::<String>();
    format!("remuda-bg-{suffix}")
}

fn pin_source(source: &BinarySource) -> DriverResult<BinaryPin> {
    match source {
        BinarySource::Pinned(pin) => Ok(pin.clone()),
        BinarySource::Path(path) => pin_binary(path),
        BinarySource::Command(name) => pin_binary(name),
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
