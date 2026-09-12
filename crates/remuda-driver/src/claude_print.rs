//! `claude-print` driver: materialize, spawn stream-json, map stdout to Observations.
//!
//! Node writes the journal. This driver exposes Observations on [`RunHandle`].
//! `TODO(M0-08)`: ingest the same events through `remuda-journal::Source`.

use crate::binary::BinaryPin;
use crate::capabilities::{ADAPTER_VERSION, capability_snapshot};
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::materializer::{BinarySource, MaterializeRequest, SessionAction, materialize};
use crate::profile::{EnvFileSecretBroker, ProviderProfile, SecretBroker};
use crate::recipe::{EnvAllowlistSource, LaunchRecipe};
use async_trait::async_trait;
use remuda_claude_wire::{
    AssistantMessage, CanUseToolRequest, ClaudeProcess, ControlRequest, ControlRequestEnvelope,
    ControlSuccessPayload, Inbound, InitializeRequest, Outbound, PermissionResult, ResultMessage,
    StreamEventMessage, SystemInit, SystemMessage, TaskNotification, TaskProgress, TaskStarted,
    TaskUpdated, UserContent, UserMessage,
};
use remuda_protocol::{
    ApprovalRequest, ClaudePermissionMode, Completeness, ContentBlock, ContentStatus, Cost,
    DecisionEffect, DecisionOption, Digest, DriverInput, DriverKind, EventId, HostId, Id,
    InputAccounting, InputOrigin, InstanceId, InstanceSpec, Interaction, InteractionAnswer,
    InteractionCarrier, InteractionId, InteractionKind, InteractionRequest, InteractionRequestKey,
    InteractionState, Knowledge, LifecyclePayload, LifecycleTopic, MessagePayload, MessagePhase,
    MessageRole, MutationOperation, NativeLifecycle, NativeRef, NativeRequestKey,
    NativeRequestValueType, NodeMutation, Observation, ObservationPayload, ObservationSource,
    OpaqueImpact, OpaquePayload, OpaqueReason, PermissionMode, QuestionField, QuestionInput,
    QuestionOption, QuestionRequest, RawRef, Redaction, ResultStage, RunId, RuntimeCursor,
    SchemaVersion, Severity, SourceChannel, SourceCursor, SourceDelivery, TextBlock,
    ThoughtPayload, ThoughtRepresentation, Timestamp, ToolCallPayload, ToolCallState, ToolCategory,
    ToolOutcome, ToolResultPayload, U64, UsageMode, UsagePayload, UsageScope, WorkflowEngine,
    WorkflowPhasePayload, WorkflowRunPayload, WorkflowState,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// How M0 answers `can_use_tool` when the host is not in the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionPolicy {
    /// `--permission-mode bypassPermissions`.
    AutoAllow,
    /// M0 `dontAsk` preset (`TD-M0-PERM-01`).
    AutoDeny,
    /// `default` + host/stdio; wait for [`Driver::respond_interaction`].
    Host,
}

/// Construction options for [`ClaudePrintDriver`].
pub struct ClaudePrintOptions {
    /// Provider profile used at materialize time.
    pub profile: ProviderProfile,
    /// Directory for 0700 launch files.
    pub launch_dir: PathBuf,
    /// Registered `CLAUDE_CONFIG_DIR`.
    pub native_home: PathBuf,
    /// Let Claude resolve its default config directory instead of exporting
    /// `CLAUDE_CONFIG_DIR`.
    ///
    /// This is opt-in because it exposes the host user's native Claude login
    /// and settings to the launched process.
    pub inherit_default_config: bool,
    /// Binary to pin and exec.
    pub binary: BinarySource,
    /// Launch origin; bot/agent cannot request bypass (D-011).
    pub origin: InputOrigin,
    /// Secret resolver.
    pub broker: Arc<dyn SecretBroker>,
    /// Extra env (tests set `FAKE_CLAUDE_SCRIPT` here).
    pub extra_env: std::collections::BTreeMap<String, String>,
    /// Override `--setting-sources`.
    pub setting_sources: Option<Vec<String>>,
    /// Initialize handshake timeout.
    pub handshake_timeout: Duration,
    /// Host-validated `--settings` overlay. Contents are never logged.
    pub settings_overlay_path: Option<PathBuf>,
}

impl ClaudePrintOptions {
    /// Human-originated print driver with the env/file broker.
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
            inherit_default_config: false,
            binary,
            origin: InputOrigin::Human,
            broker: Arc::new(EnvFileSecretBroker),
            extra_env: std::collections::BTreeMap::new(),
            setting_sources: None,
            handshake_timeout: Duration::from_secs(30),
            settings_overlay_path: None,
        }
    }
}

struct PendingTool {
    request_id: String,
    input: Value,
}

struct Live {
    process: ClaudeProcess,
    inbound: mpsc::Sender<Inbound>,
    pending: HashMap<InteractionId, PendingTool>,
    pending_by_native: HashMap<String, InteractionId>,
    recipe: LaunchRecipe,
}

struct Mapper {
    ids: NativeIds,
    seq: u64,
    instance_id: InstanceId,
    run_id: RunId,
    journal_id: Id,
    host_id: HostId,
    session_id: String,
    pin: BinaryPin,
}

#[derive(Default)]
struct NativeIds {
    messages: HashMap<String, (Id, u64)>,
    tools: HashMap<String, Id>,
    thoughts: HashMap<String, Id>,
    workflows: HashMap<String, Id>,
    phases: HashMap<String, Id>,
}

impl NativeIds {
    fn message(&mut self, native: &str) -> DriverResult<(Id, U64, MutationOperation)> {
        if let Some((id, rev)) = self.messages.get_mut(native) {
            *rev += 1;
            return Ok((id.clone(), U64(*rev), MutationOperation::Append));
        }
        let id = Id::new("obj")?;
        self.messages.insert(native.to_owned(), (id.clone(), 1));
        Ok((id, U64(1), MutationOperation::Open))
    }

    fn tool(&mut self, native: &str) -> DriverResult<Id> {
        if let Some(id) = self.tools.get(native) {
            return Ok(id.clone());
        }
        let id = Id::new("obj")?;
        self.tools.insert(native.to_owned(), id.clone());
        Ok(id)
    }

    fn thought(&mut self, native: &str) -> DriverResult<Id> {
        if let Some(id) = self.thoughts.get(native) {
            return Ok(id.clone());
        }
        let id = Id::new("obj")?;
        self.thoughts.insert(native.to_owned(), id.clone());
        Ok(id)
    }

    fn workflow(&mut self, native: &str) -> DriverResult<Id> {
        if let Some(id) = self.workflows.get(native) {
            return Ok(id.clone());
        }
        let id = Id::new("obj")?;
        self.workflows.insert(native.to_owned(), id.clone());
        Ok(id)
    }

    fn phase(&mut self, native: &str) -> DriverResult<Id> {
        if let Some(id) = self.phases.get(native) {
            return Ok(id.clone());
        }
        let id = Id::new("obj")?;
        self.phases.insert(native.to_owned(), id.clone());
        Ok(id)
    }
}

struct Inner {
    live: Mutex<Option<Live>>,
    mapper: Mutex<Mapper>,
    policy: Mutex<PermissionPolicy>,
    events: Mutex<Option<mpsc::Sender<Observation>>>,
    closed: AtomicBool,
    /// Last successfully launched spec; `resume` re-materializes from this.
    last_spec: Mutex<Option<InstanceSpec>>,
}

/// Native Claude print driver (`claude -p` stream-json).
pub struct ClaudePrintDriver {
    options: ClaudePrintOptions,
    inner: Arc<Inner>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl ClaudePrintDriver {
    /// Build a driver from explicit options.
    pub fn new(options: ClaudePrintOptions) -> Self {
        Self {
            options,
            inner: Arc::new(Inner {
                live: Mutex::new(None),
                mapper: Mutex::new(Mapper {
                    ids: NativeIds::default(),
                    seq: 0,
                    instance_id: InstanceId::new(),
                    run_id: RunId::new(),
                    journal_id: fallback_obj(),
                    host_id: fallback_host(),
                    session_id: String::new(),
                    pin: BinaryPin {
                        abs_path: String::new(),
                        version: String::new(),
                        sha256: dummy_digest(),
                    },
                }),
                policy: Mutex::new(PermissionPolicy::Host),
                events: Mutex::new(None),
                closed: AtomicBool::new(false),
                last_spec: Mutex::new(None),
            }),
            reader: Mutex::new(None),
        }
    }

    /// SIGKILL the child. Stdout EOF then emits a session-exited lifecycle.
    pub async fn kill(&self) -> DriverResult<DriverAck> {
        let mut live = self.inner.live.lock().await;
        let Some(live) = live.as_mut() else {
            return Err(DriverError::ControlUnavailable);
        };
        live.process.kill().map_err(map_wire)?;
        Ok(DriverAck::transport_written())
    }

    async fn launch(&self, spec: InstanceSpec, session: SessionAction) -> DriverResult<RunHandle> {
        if spec.driver != DriverKind::ClaudePrint {
            return Err(DriverError::InvalidLaunchSpec(
                "ClaudePrintDriver requires driverKind claude-print".into(),
            ));
        }
        reject_bot_bypass(&spec, self.options.origin)?;
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
            origin: self.options.origin.into(),
            settings_overlay_path: self.options.settings_overlay_path.clone(),
        };
        let mut recipe = materialize(&request)?;
        apply_bypass_flag(&mut recipe);
        refuse_prohibited_argv(&recipe.argv)?;
        let policy = policy_from_recipe(&recipe, &spec);
        let env = self.resolve_env(&spec, &recipe).await?;
        let mut command = Command::new(&recipe.binary.abs_path);
        command.args(&recipe.argv).current_dir(&recipe.cwd);
        for (key, value) in &env {
            command.env(key, value);
        }
        for (key, value) in &self.options.extra_env {
            command.env(key, value);
        }
        configure_native_home(
            &mut command,
            &recipe.native_home,
            self.options.inherit_default_config,
        );

        let (inbound, outbound, process) = ClaudeProcess::spawn_command(
            command,
            Path::new(&recipe.binary.abs_path),
            InitializeRequest::default(),
            self.options.handshake_timeout,
            remuda_claude_wire::DEFAULT_MAX_LINE_BYTES,
        )
        .await
        .map_err(map_wire)?;

        let instance_id = InstanceId::new();
        let run_id = RunId::new();
        let journal_id = Id::new("obj")?;
        let (tx, rx) = mpsc::channel(256);
        {
            let mut mapper = self.inner.mapper.lock().await;
            *mapper = Mapper {
                ids: NativeIds::default(),
                seq: 0,
                instance_id: instance_id.clone(),
                run_id,
                journal_id,
                host_id: spec.host.clone(),
                session_id: session_id.clone(),
                pin: recipe.binary.clone(),
            };
        }
        *self.inner.policy.lock().await = policy;
        *self.inner.events.lock().await = Some(tx);
        *self.inner.last_spec.lock().await = Some(spec.clone());
        self.inner.closed.store(false, Ordering::SeqCst);

        let mut ack = DriverAck::transport_written();
        ack.native_ids.insert("sessionId".into(), session_id);
        if let Some(pid) = process.id() {
            ack.native_ids.insert("pid".into(), pid.to_string());
        }

        *self.inner.live.lock().await = Some(Live {
            process,
            inbound,
            pending: HashMap::new(),
            pending_by_native: HashMap::new(),
            recipe: recipe.clone(),
        });

        let inner = Arc::clone(&self.inner);
        let reader = tokio::spawn(async move {
            map_loop(inner, outbound).await;
        });
        *self.reader.lock().await = Some(reader);
        Ok(RunHandle::new(recipe, ack, rx))
    }

    async fn resolve_env(
        &self,
        spec: &InstanceSpec,
        recipe: &LaunchRecipe,
    ) -> DriverResult<std::collections::BTreeMap<String, String>> {
        let mut env = std::collections::BTreeMap::new();
        for entry in &recipe.env_allowlist {
            match entry.source {
                EnvAllowlistSource::NativeHome => {
                    env.insert(entry.name.clone(), recipe.native_home.clone());
                }
                EnvAllowlistSource::ProviderOverlay => {
                    if entry.name == "ANTHROPIC_BASE_URL" && !recipe.provider.base_url.is_empty() {
                        env.insert(entry.name.clone(), recipe.provider.base_url.clone());
                    }
                }
                EnvAllowlistSource::Credential => {
                    let raw = entry.secret_ref.clone().or_else(|| {
                        self.options
                            .profile
                            .secret_ref
                            .as_ref()
                            .map(|secret| secret.as_str().to_string())
                    });
                    let Some(raw) = raw else {
                        continue;
                    };
                    if raw.starts_with("helper:") {
                        continue;
                    }
                    let Ok(secret_ref) = crate::SecretRef::parse(raw) else {
                        continue;
                    };
                    match self.options.broker.resolve(&secret_ref).await {
                        Ok(secret) => {
                            env.insert(entry.name.clone(), secret.expose_str()?.to_owned());
                        }
                        Err(error) => {
                            debug!(%error, name = %entry.name, "credential unresolved");
                        }
                    }
                }
                EnvAllowlistSource::Literal => {
                    if let Some(remuda_protocol::EnvBinding::Literal(lit)) =
                        spec.env.get(&entry.name)
                    {
                        env.insert(entry.name.clone(), lit.value.clone());
                    }
                }
                EnvAllowlistSource::HostEnv => {
                    if let Ok(value) = std::env::var(&entry.name) {
                        env.insert(entry.name.clone(), value);
                    }
                }
            }
        }
        Ok(env)
    }
}

fn configure_native_home(command: &mut Command, native_home: &str, inherit_default: bool) {
    // Claude Code's macOS login lookup changes namespaces when this variable is
    // present, even when it names the default ~/.claude directory. Omit it only
    // for the explicit host-login opt-in; isolated homes remain the default.
    if inherit_default {
        command.env_remove("CLAUDE_CONFIG_DIR");
    } else {
        command.env("CLAUDE_CONFIG_DIR", native_home);
    }
}

#[cfg(test)]
mod native_home_tests {
    use super::*;
    use std::ffi::OsStr;

    fn configured_value(command: &Command) -> Option<Option<&OsStr>> {
        command
            .as_std()
            .get_envs()
            .find_map(|(key, value)| (key == "CLAUDE_CONFIG_DIR").then_some(value))
    }

    #[test]
    fn isolated_home_is_exported() {
        let mut command = Command::new("claude");
        configure_native_home(&mut command, "/tmp/isolated-claude", false);
        assert_eq!(
            configured_value(&command).flatten(),
            Some(OsStr::new("/tmp/isolated-claude"))
        );
    }

    #[test]
    fn inherited_default_does_not_override_environment() {
        let mut command = Command::new("claude");
        // The materialized NativeHome allowlist is applied before this final
        // policy step. Ensure the opt-in removes that already-set value.
        command.env("CLAUDE_CONFIG_DIR", "/tmp/materialized-home");
        configure_native_home(&mut command, "/tmp/not-exported", true);
        assert_eq!(configured_value(&command), Some(None));
    }
}

#[async_trait]
impl Driver for ClaudePrintDriver {
    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        let live = self.inner.live.lock().await;
        let pin = live
            .as_ref()
            .map(|live| live.recipe.binary.clone())
            .unwrap_or_else(|| BinaryPin {
                abs_path: String::new(),
                version: "unpinned".into(),
                sha256: dummy_digest(),
            });
        Ok(capability_snapshot(
            DriverKind::ClaudePrint,
            &pin,
            U64(1),
            U64(1),
        )?)
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        let session_id = uuid::Uuid::new_v4().to_string();
        self.launch(spec, SessionAction::New { session_id }).await
    }

    async fn attach(&self, _native_ref: NativeRef) -> DriverResult<DriverAck> {
        Err(DriverError::ControlUnavailable)
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        let live = self.inner.live.lock().await;
        let Some(live) = live.as_ref() else {
            return Err(DriverError::ControlUnavailable);
        };
        match input {
            DriverInput::Prompt(prompt) => {
                let text = prompt_text(&prompt.blocks)?;
                live.process
                    .send_user(UserContent::Text(text))
                    .await
                    .map_err(map_wire)?;
                Ok(DriverAck::transport_written())
            }
            DriverInput::Steer(_) => Err(DriverError::CapabilityUnknown("steer".into())),
            DriverInput::ModelSwitch(switch) => {
                live.process
                    .set_model(Some(switch.model_id.clone()))
                    .await
                    .map_err(map_wire)?;
                Ok(DriverAck::transport_written())
            }
        }
    }

    async fn cancel(&self) -> DriverResult<DriverAck> {
        let live = self.inner.live.lock().await;
        let Some(live) = live.as_ref() else {
            return Err(DriverError::ControlUnavailable);
        };
        live.process.interrupt(true).await.map_err(map_wire)?;
        Ok(DriverAck::transport_written())
    }

    async fn respond_interaction(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        let mut live = self.inner.live.lock().await;
        let Some(live) = live.as_mut() else {
            return Err(DriverError::ControlUnavailable);
        };
        let pending = live
            .pending
            .remove(&id)
            .ok_or(DriverError::ControlUnavailable)?;
        live.pending_by_native.remove(&pending.request_id);
        let payload = permission_from_answer(&pending.input, &answer)?;
        live.inbound
            .send(Inbound::control_success(pending.request_id, payload))
            .await
            .map_err(|_| DriverError::ControlUnavailable)?;
        Ok(DriverAck::transport_written())
    }

    async fn close(&self) -> DriverResult<DriverAck> {
        self.inner.closed.store(true, Ordering::SeqCst);
        let mut live_guard = self.inner.live.lock().await;
        if let Some(live) = live_guard.as_mut() {
            let _ = live.process.close_stdin().await;
            let _ = live.process.wait().await;
        }
        *live_guard = None;
        *self.inner.events.lock().await = None;
        Ok(DriverAck::not_dispatched())
    }

    async fn resume(&self, native_ref: NativeRef) -> DriverResult<RunHandle> {
        let session_id = match native_ref
            .claude
            .as_ref()
            .map(|claude| claude.session_id.clone())
            .or_else(|| match &native_ref.session_id {
                Knowledge::Known { value } => Some(value.clone()),
                _ => None,
            }) {
            Some(id) if !id.is_empty() => id,
            _ => return Err(DriverError::NativeSessionNotFound),
        };
        let mut spec = self
            .inner
            .last_spec
            .lock()
            .await
            .clone()
            .ok_or(DriverError::NativeSessionNotFound)?;
        spec.host = native_ref.host_id.clone();
        spec.driver = DriverKind::ClaudePrint;
        spec.kind = remuda_protocol::AgentKind::Claude;
        self.launch(spec, SessionAction::Resume { session_id })
            .await
    }
}

async fn map_loop(inner: Arc<Inner>, mut outbound: mpsc::Receiver<Outbound>) {
    while let Some(frame) = outbound.recv().await {
        if let Err(error) = handle_frame(&inner, frame).await {
            warn!(%error, "claude-print map failed");
        }
    }
    if let Err(error) = emit_exit(&inner, "exited").await {
        debug!(%error, "claude-print exit lifecycle");
    }
}

async fn handle_frame(inner: &Inner, frame: Outbound) -> DriverResult<()> {
    if matches!(frame, Outbound::KeepAlive) {
        return Ok(());
    }
    if let Outbound::ControlRequest(env) = &frame
        && let ControlRequest::CanUseTool(req) = &env.request
    {
        return handle_can_use_tool(inner, env, req).await;
    }
    let observations = {
        let mut mapper = inner.mapper.lock().await;
        map_outbound(&mut mapper, &frame)?
    };
    emit_all(inner, observations).await
}

async fn handle_can_use_tool(
    inner: &Inner,
    env: &ControlRequestEnvelope,
    req: &CanUseToolRequest,
) -> DriverResult<()> {
    let policy = *inner.policy.lock().await;
    let interaction_id = InteractionId::new();
    let request_id = env.request_id.clone();
    let observations = {
        let mut mapper = inner.mapper.lock().await;
        let interaction = build_interaction(&mut mapper, env, req, interaction_id.clone())?;
        vec![mapper.observation(
            Completeness::Structured,
            NativeRequestKey::Rpc {
                value_type: NativeRequestValueType::String,
                value: request_id,
            },
            ObservationPayload::InteractionRequested(Box::new(
                remuda_protocol::InteractionRequestedPayload { interaction },
            )),
        )?]
    };
    emit_all(inner, observations).await?;

    match policy {
        PermissionPolicy::AutoAllow => {
            let payload = ControlSuccessPayload::Permission(PermissionResult::Allow {
                updated_input: req.input.clone(),
                updated_permissions: None,
            });
            send_control(inner, &env.request_id, payload).await?;
        }
        PermissionPolicy::AutoDeny => {
            let payload = ControlSuccessPayload::Permission(PermissionResult::Deny {
                message: "dontAsk policy denied this tool call".into(),
                interrupt: Some(false),
            });
            send_control(inner, &env.request_id, payload).await?;
        }
        PermissionPolicy::Host => {
            let mut live = inner.live.lock().await;
            if let Some(live) = live.as_mut() {
                live.pending.insert(
                    interaction_id.clone(),
                    PendingTool {
                        request_id: env.request_id.clone(),
                        input: req.input.clone(),
                    },
                );
                live.pending_by_native
                    .insert(env.request_id.clone(), interaction_id);
            }
        }
    }
    Ok(())
}

async fn send_control(
    inner: &Inner,
    request_id: &str,
    payload: ControlSuccessPayload,
) -> DriverResult<()> {
    let live = inner.live.lock().await;
    let Some(live) = live.as_ref() else {
        return Err(DriverError::ControlUnavailable);
    };
    live.inbound
        .send(Inbound::control_success(request_id, payload))
        .await
        .map_err(|_| DriverError::ControlUnavailable)
}

async fn emit_all(inner: &Inner, observations: Vec<Observation>) -> DriverResult<()> {
    let tx = inner.events.lock().await.clone();
    let Some(tx) = tx else {
        return Ok(());
    };
    for observation in observations {
        if tx.send(observation).await.is_err() {
            break;
        }
    }
    Ok(())
}

async fn emit_exit(inner: &Inner, status: &str) -> DriverResult<()> {
    if inner.closed.load(Ordering::SeqCst) {
        // still emit
    }
    let observation = {
        let mut mapper = inner.mapper.lock().await;
        let session_id = mapper.session_id.clone();
        mapper.lifecycle(
            LifecycleTopic::Session,
            "session",
            Knowledge::Known { value: session_id },
            status,
            false,
        )?
    };
    emit_all(inner, vec![observation]).await
}

fn map_outbound(mapper: &mut Mapper, frame: &Outbound) -> DriverResult<Vec<Observation>> {
    match frame {
        Outbound::KeepAlive => Ok(Vec::new()),
        Outbound::System(system) => map_system(mapper, system),
        Outbound::Assistant(msg) => map_assistant(mapper, msg),
        Outbound::User(msg) => map_user(mapper, msg),
        Outbound::Result(result) => map_result(mapper, result),
        Outbound::StreamEvent(event) => map_stream(mapper, event),
        Outbound::ControlRequest(_) | Outbound::ControlResponse(_) => Ok(Vec::new()),
        Outbound::ControlCancelRequest { .. } => Ok(Vec::new()),
        Outbound::RateLimitEvent(_) => {
            mapper.lifecycle_named(LifecycleTopic::Diagnostic, "rate_limit_event")
        }
        Outbound::ToolProgress(_) => mapper.lifecycle_named(LifecycleTopic::Task, "tool_progress"),
        Outbound::PromptSuggestion(_) => {
            mapper.lifecycle_named(LifecycleTopic::Diagnostic, "prompt_suggestion")
        }
        Outbound::Unknown(value) => mapper.opaque("unknown-type", OpaqueReason::UnknownType, value),
    }
}

fn map_system(mapper: &mut Mapper, system: &SystemMessage) -> DriverResult<Vec<Observation>> {
    match system {
        SystemMessage::Init(init) => map_init(mapper, init),
        SystemMessage::HookStarted(hook) => mapper.lifecycle_hook(
            "hook_started",
            hook.hook_id.as_deref().unwrap_or(""),
            hook.hook_event.as_deref().unwrap_or(""),
        ),
        SystemMessage::HookProgress(hook) => mapper.lifecycle_hook(
            "hook_progress",
            hook.hook_id.as_deref().unwrap_or(""),
            hook.hook_event.as_deref().unwrap_or(""),
        ),
        SystemMessage::HookResponse(hook) => mapper.lifecycle_hook(
            "hook_response",
            hook.hook_id.as_deref().unwrap_or(""),
            hook.hook_event.as_deref().unwrap_or(""),
        ),
        SystemMessage::TaskStarted(task) => map_task_started(mapper, task),
        SystemMessage::TaskProgress(task) => map_task_progress(mapper, task),
        SystemMessage::TaskUpdated(task) => map_task_updated(mapper, task),
        SystemMessage::TaskNotification(task) => map_task_notification(mapper, task),
        SystemMessage::BackgroundTasksChanged(_) => {
            mapper.lifecycle_named(LifecycleTopic::Task, "background_tasks_changed")
        }
        SystemMessage::PermissionDenied(_) => {
            mapper.lifecycle_named(LifecycleTopic::Permission, "permission_denied")
        }
        SystemMessage::ThinkingTokens(_) => {
            mapper.lifecycle_named(LifecycleTopic::Diagnostic, "thinking_tokens")
        }
        SystemMessage::ApiRetry(_) => {
            mapper.lifecycle_named(LifecycleTopic::Diagnostic, "api_retry")
        }
        SystemMessage::SessionStateChanged(_) => {
            mapper.lifecycle_named(LifecycleTopic::Session, "session_state_changed")
        }
        SystemMessage::Unknown(value) => mapper.opaque("system", OpaqueReason::UnknownType, value),
        other => mapper.lifecycle_named(LifecycleTopic::Diagnostic, other.subtype_name()),
    }
}

fn map_init(mapper: &mut Mapper, init: &SystemInit) -> DriverResult<Vec<Observation>> {
    if let Some(session) = &init.session_id {
        mapper.session_id.clone_from(session);
    }
    let session_id = mapper.session_id.clone();
    let mut related = std::collections::BTreeMap::new();
    if let Some(tools) = &init.tools {
        related.insert("tools".into(), tools.join(","));
    }
    if let Some(caps) = &init.capabilities {
        related.insert("capabilities".into(), caps.join(","));
    }
    if let Some(model) = &init.model {
        related.insert("model".into(), model.clone());
    }
    Ok(vec![mapper.lifecycle_related(
        LifecycleTopic::Session,
        "session",
        Knowledge::Known { value: session_id },
        "started",
        related,
        false,
    )?])
}

fn map_assistant(mapper: &mut Mapper, msg: &AssistantMessage) -> DriverResult<Vec<Observation>> {
    let native_msg = msg
        .message
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| msg.uuid.clone());
    let content = msg
        .message
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    let has_tool = content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"));
    for (index, block) in content.iter().enumerate() {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "thinking" | "redacted_thinking" => {
                let key = native_msg
                    .clone()
                    .unwrap_or_else(|| format!("thought-{index}"));
                let id = mapper.ids.thought(&key)?;
                let redacted =
                    block.get("type").and_then(Value::as_str) == Some("redacted_thinking");
                out.push(
                    mapper.observation(
                        Completeness::Structured,
                        NativeRequestKey::None,
                        ObservationPayload::Thought(Box::new(ThoughtPayload {
                            mutation: NodeMutation {
                                node_id: id.clone(),
                                revision: U64(1),
                                operation: MutationOperation::Open,
                                base_revision: None,
                            },
                            thought_id: id,
                            representation: if redacted {
                                ThoughtRepresentation::Redacted
                            } else {
                                ThoughtRepresentation::Text
                            },
                            text: block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            part_index: index as u32,
                            status: ContentStatus::Complete,
                        })),
                    )?,
                );
            }
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                let key = native_msg.clone().unwrap_or_else(|| format!("msg-{index}"));
                let (id, rev, op) = mapper.ids.message(&key)?;
                out.push(mapper.observation(
                    Completeness::Structured,
                    NativeRequestKey::None,
                    ObservationPayload::Message(Box::new(MessagePayload {
                        mutation: NodeMutation {
                            node_id: id.clone(),
                            revision: rev,
                            operation: op,
                            base_revision: None,
                        },
                        message_id: id,
                        role: MessageRole::Assistant,
                        phase: if has_tool {
                            MessagePhase::Commentary
                        } else {
                            MessagePhase::Final
                        },
                        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                            text: text.to_owned(),
                        }))],
                        target_block: None,
                        parent_tool_call_id: None,
                        native_origin: known_or_unknown(native_msg.as_deref()),
                        status: ContentStatus::Complete,
                    })),
                )?);
            }
            "tool_use" => {
                let tool_use_id = block.get("id").and_then(Value::as_str).unwrap_or("");
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let id = mapper.ids.tool(tool_use_id)?;
                out.push(mapper.observation(
                    Completeness::Structured,
                    NativeRequestKey::None,
                    ObservationPayload::ToolCall(Box::new(ToolCallPayload {
                        mutation: NodeMutation {
                            node_id: id.clone(),
                            revision: U64(1),
                            operation: MutationOperation::Open,
                            base_revision: None,
                        },
                        tool_call_id: id,
                        parent_tool_call_id: None,
                        tool_name: Knowledge::Known {
                            value: name.to_owned(),
                        },
                        display_title: Knowledge::Known {
                            value: name.to_owned(),
                        },
                        category: tool_category(name),
                        input: match block.get("input") {
                            Some(input) => Knowledge::Known {
                                value: input.clone(),
                            },
                            None => unknown("missing-input"),
                        },
                        input_text_delta: None,
                        state: ToolCallState::Proposed,
                        executor: Knowledge::NotApplicable,
                    })),
                )?);
            }
            other => {
                out.extend(mapper.opaque(other, OpaqueReason::UnmappedFields, block)?);
            }
        }
    }
    Ok(out)
}

fn map_user(mapper: &mut Mapper, msg: &UserMessage) -> DriverResult<Vec<Observation>> {
    let mut out = Vec::new();
    match &msg.message.content {
        UserContent::Text(text) => {
            let key = msg.uuid.clone().unwrap_or_else(|| "user".into());
            let (id, rev, op) = mapper.ids.message(&key)?;
            out.push(mapper.observation(
                Completeness::Structured,
                NativeRequestKey::None,
                ObservationPayload::Message(Box::new(MessagePayload {
                    mutation: NodeMutation {
                        node_id: id.clone(),
                        revision: rev,
                        operation: op,
                        base_revision: None,
                    },
                    message_id: id,
                    role: MessageRole::User,
                    phase: MessagePhase::Input,
                    blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                        text: text.clone(),
                    }))],
                    target_block: None,
                    parent_tool_call_id: None,
                    native_origin: known_or_unknown(msg.uuid.as_deref()),
                    status: ContentStatus::Complete,
                })),
            )?);
        }
        UserContent::Blocks(blocks) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let tool_use_id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let id = mapper.ids.tool(tool_use_id)?;
                    let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
                    let text = match block.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    out.push(mapper.observation(
                        Completeness::Structured,
                        NativeRequestKey::None,
                        ObservationPayload::ToolResult(Box::new(ToolResultPayload {
                            mutation: NodeMutation {
                                node_id: id.clone(),
                                revision: U64(1),
                                operation: MutationOperation::Open,
                                base_revision: None,
                            },
                            tool_call_id: id,
                            stage: ResultStage::Final,
                            outcome: if is_error {
                                ToolOutcome::Failed
                            } else {
                                ToolOutcome::Succeeded
                            },
                            blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
                            structured_result: Knowledge::NotApplicable,
                            exit_code: Knowledge::NotApplicable,
                            changes: Vec::new(),
                        })),
                    )?);
                }
            }
        }
    }
    Ok(out)
}

fn map_result(mapper: &mut Mapper, result: &ResultMessage) -> DriverResult<Vec<Observation>> {
    let mut related = std::collections::BTreeMap::new();
    if let Some(index) = result.result_index {
        related.insert("resultIndex".into(), index.to_string());
    }
    if let Some(turns) = result.num_turns {
        related.insert("numTurns".into(), turns.to_string());
    }
    let status = if result.is_error == Some(true) {
        "error"
    } else {
        "turn_done"
    };
    // Workflow emits result_index 0 then 1; the first is not process completion
    // (stream-json §5: do not tear down on the first result).
    let affects_completion =
        result.result_index.unwrap_or(0) > 0 && result.queued_turn_count.unwrap_or(0) == 0;
    let session_id = mapper.session_id.clone();
    let mut out = vec![mapper.lifecycle_related(
        LifecycleTopic::Turn,
        "result",
        Knowledge::Known { value: session_id },
        status,
        related,
        affects_completion,
    )?];
    if let Some(usage) = usage_from_result(mapper, result)? {
        out.push(usage);
    }
    Ok(out)
}

fn usage_from_result(
    mapper: &mut Mapper,
    result: &ResultMessage,
) -> DriverResult<Option<Observation>> {
    let usage = result.usage.clone().unwrap_or(Value::Null);
    let input = usage.get("input_tokens").and_then(Value::as_u64);
    let output = usage.get("output_tokens").and_then(Value::as_u64);
    if input.is_none() && output.is_none() && result.total_cost_usd.is_none() {
        return Ok(None);
    }
    let cost = result.total_cost_usd.map(|amount| Cost {
        amount: amount.to_string(),
        currency: "USD".into(),
    });
    Ok(Some(
        mapper.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            ObservationPayload::Usage(Box::new(UsagePayload {
                usage_id: Id::new("obj")?,
                scope: UsageScope::Turn,
                scope_id: result
                    .result_index
                    .map(|index| index.to_string())
                    .unwrap_or_else(|| "0".into()),
                mode: UsageMode::Snapshot,
                metric_revision: U64(result.result_index.unwrap_or(0) + 1),
                input_tokens: opt_u64(input),
                input_accounting: InputAccounting::Unknown,
                output_tokens: opt_u64(output),
                reasoning_tokens: unknown("not-emitted"),
                cache_read_tokens: unknown("not-emitted"),
                cache_write_tokens: unknown("not-emitted"),
                total_tokens: match (input, output) {
                    (Some(a), Some(b)) => Knowledge::Known { value: U64(a + b) },
                    _ => unknown("partial"),
                },
                cost: match cost {
                    Some(cost) => Knowledge::Known { value: cost },
                    None => unknown("not-emitted"),
                },
                accounting: remuda_protocol::Accounting::Reported,
                native_fields_ref: None,
            })),
        )?,
    ))
}

fn map_stream(mapper: &mut Mapper, event: &StreamEventMessage) -> DriverResult<Vec<Observation>> {
    let delta = event
        .event
        .pointer("/delta/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    if delta.is_empty() {
        return Ok(Vec::new());
    }
    let key = event.uuid.clone().unwrap_or_else(|| "stream".into());
    let (id, rev, op) = mapper.ids.message(&key)?;
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::Message(Box::new(MessagePayload {
            mutation: NodeMutation {
                node_id: id.clone(),
                revision: rev,
                operation: op,
                base_revision: None,
            },
            message_id: id,
            role: MessageRole::Assistant,
            phase: MessagePhase::Final,
            blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                text: delta.to_owned(),
            }))],
            target_block: Some(0),
            parent_tool_call_id: None,
            native_origin: known_or_unknown(event.uuid.as_deref()),
            status: ContentStatus::Streaming,
        })),
    )?])
}

fn map_task_started(mapper: &mut Mapper, task: &TaskStarted) -> DriverResult<Vec<Observation>> {
    let task_id = task.task_id.clone().unwrap_or_default();
    let workflow_id = mapper.ids.workflow(&task_id)?;
    let tool_call_id = match &task.tool_use_id {
        Some(id) => Some(mapper.ids.tool(id)?),
        None => None,
    };
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::WorkflowRun(Box::new(WorkflowRunPayload {
            workflow_id,
            engine: WorkflowEngine::ClaudeWorkflow,
            native_run_id: unknown("not-emitted"),
            native_task_id: Knowledge::Known { value: task_id },
            tool_call_id,
            state: WorkflowState::Running,
            revision: U64(1),
            title: match &task.description {
                Some(text) => Knowledge::Known {
                    value: text.clone(),
                },
                None => unknown("not-emitted"),
            },
            result_ref: None,
        })),
    )?])
}

fn map_task_progress(mapper: &mut Mapper, task: &TaskProgress) -> DriverResult<Vec<Observation>> {
    let task_id = task.task_id.clone().unwrap_or_default();
    let workflow_id = mapper.ids.workflow(&task_id)?;
    let phase_id = mapper.ids.phase(&format!("{task_id}-progress"))?;
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::WorkflowPhase(Box::new(WorkflowPhasePayload {
            workflow_id,
            phase_id,
            native_phase_id: Knowledge::Known {
                value: "progress".into(),
            },
            label: match &task.summary {
                Some(text) => Knowledge::Known {
                    value: text.clone(),
                },
                None => unknown("not-emitted"),
            },
            state: WorkflowState::Running,
            revision: U64(1),
            parent_phase_id: None,
        })),
    )?])
}

fn map_task_updated(mapper: &mut Mapper, task: &TaskUpdated) -> DriverResult<Vec<Observation>> {
    let task_id = task.task_id.clone().unwrap_or_default();
    let workflow_id = mapper.ids.workflow(&task_id)?;
    let status = task
        .patch
        .as_ref()
        .and_then(|patch| patch.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::WorkflowRun(Box::new(WorkflowRunPayload {
            workflow_id,
            engine: WorkflowEngine::ClaudeWorkflow,
            native_run_id: unknown("not-emitted"),
            native_task_id: Knowledge::Known { value: task_id },
            tool_call_id: None,
            state: workflow_state(status),
            revision: U64(2),
            title: unknown("not-emitted"),
            result_ref: None,
        })),
    )?])
}

fn map_task_notification(
    mapper: &mut Mapper,
    task: &TaskNotification,
) -> DriverResult<Vec<Observation>> {
    let task_id = task.task_id.clone().unwrap_or_default();
    let workflow_id = mapper.ids.workflow(&task_id)?;
    let tool_call_id = match &task.tool_use_id {
        Some(id) => Some(mapper.ids.tool(id)?),
        None => None,
    };
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::WorkflowRun(Box::new(WorkflowRunPayload {
            workflow_id,
            engine: WorkflowEngine::ClaudeWorkflow,
            native_run_id: unknown("not-emitted"),
            native_task_id: Knowledge::Known { value: task_id },
            tool_call_id,
            state: workflow_state(task.status.as_deref().unwrap_or("completed")),
            revision: U64(3),
            title: match &task.summary {
                Some(text) => Knowledge::Known {
                    value: text.clone(),
                },
                None => unknown("not-emitted"),
            },
            result_ref: None,
        })),
    )?])
}

impl Mapper {
    fn observation(
        &mut self,
        completeness: Completeness,
        native_request_id: NativeRequestKey,
        body: ObservationPayload,
    ) -> DriverResult<Observation> {
        self.seq += 1;
        Ok(Observation {
            schema_version: SchemaVersion,
            event_id: EventId::new(),
            journal_id: self.journal_id.clone(),
            instance_id: self.instance_id.clone(),
            run_id: Some(self.run_id.clone()),
            host_id: self.host_id.clone(),
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            seq: U64(self.seq),
            observed_at: now()?,
            native_at: unknown("not-emitted"),
            source: ObservationSource {
                driver_kind: DriverKind::ClaudePrint,
                driver_version: self.pin.version.clone(),
                adapter_version: ADAPTER_VERSION.into(),
                channel: SourceChannel::Stdout,
                delivery: SourceDelivery::Live,
                native_session_id: Knowledge::Known {
                    value: self.session_id.clone(),
                },
                native_turn_id: unknown("not-emitted"),
                native_agent_id: Knowledge::NotApplicable,
                native_item_id: unknown("not-emitted"),
                native_event_id: unknown("not-emitted"),
                native_request_id,
                source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                    ledger_revision: U64(self.seq),
                })),
            },
            completeness,
            raw_ref: None,
            evidence_event_ids: vec![],
            body,
        })
    }

    fn lifecycle(
        &mut self,
        topic: LifecycleTopic,
        native_name: &str,
        native_id: Knowledge<String>,
        status: &str,
        affects_completion: bool,
    ) -> DriverResult<Observation> {
        self.lifecycle_related(
            topic,
            native_name,
            native_id,
            status,
            std::collections::BTreeMap::new(),
            affects_completion,
        )
    }

    fn lifecycle_related(
        &mut self,
        topic: LifecycleTopic,
        native_name: &str,
        native_id: Knowledge<String>,
        status: &str,
        related_ids: std::collections::BTreeMap<String, String>,
        affects_completion: bool,
    ) -> DriverResult<Observation> {
        self.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                NativeLifecycle {
                    topic,
                    native_name: native_name.into(),
                    native_id,
                    status: Knowledge::Known {
                        value: status.into(),
                    },
                    related_ids,
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion,
                },
            )))),
        )
    }

    fn lifecycle_named(
        &mut self,
        topic: LifecycleTopic,
        name: &str,
    ) -> DriverResult<Vec<Observation>> {
        let session_id = self.session_id.clone();
        Ok(vec![self.lifecycle(
            topic,
            name,
            Knowledge::Known { value: session_id },
            name,
            false,
        )?])
    }

    fn lifecycle_hook(
        &mut self,
        name: &str,
        hook_id: &str,
        hook_event: &str,
    ) -> DriverResult<Vec<Observation>> {
        let mut related = std::collections::BTreeMap::new();
        if !hook_id.is_empty() {
            related.insert("hookId".into(), hook_id.into());
        }
        if !hook_event.is_empty() {
            related.insert("hookEvent".into(), hook_event.into());
        }
        Ok(vec![self.lifecycle_related(
            LifecycleTopic::Hook,
            name,
            Knowledge::Known {
                value: hook_id.to_owned(),
            },
            name,
            related,
            false,
        )?])
    }

    fn opaque(
        &mut self,
        native_type: &str,
        reason: OpaqueReason,
        value: &Value,
    ) -> DriverResult<Vec<Observation>> {
        let bytes = serde_json::to_vec(value)?;
        let digest = crate::binary::hash_bytes(&bytes)?;
        Ok(vec![self.observation(
            Completeness::Opaque,
            NativeRequestKey::None,
            ObservationPayload::Opaque(Box::new(OpaquePayload {
                native_type: native_type.into(),
                reason,
                raw_ref: RawRef {
                    object_id: Id::new("obj")?,
                    offset: U64(0),
                    length: U64(bytes.len() as u64),
                    digest,
                    media_type: "application/json".into(),
                    redaction: Redaction::None,
                },
                affects: vec![OpaqueImpact::Presentation],
                summary: None,
            })),
        )?])
    }
}

fn build_interaction(
    mapper: &mut Mapper,
    env: &ControlRequestEnvelope,
    req: &CanUseToolRequest,
    interaction_id: InteractionId,
) -> DriverResult<Interaction> {
    let now = now()?;
    let digest = crate::binary::hash_bytes(&serde_json::to_vec(&req.input)?)?;
    let tool_call_id = match &req.tool_use_id {
        Some(id) => Some(mapper.ids.tool(id)?),
        None => None,
    };
    let ask = req.tool_name == "AskUserQuestion";
    let request = if ask {
        InteractionRequest::Question(Box::new(question_request(req)))
    } else {
        InteractionRequest::Approval(Box::new(ApprovalRequest {
            title: req
                .display_name
                .clone()
                .unwrap_or_else(|| req.tool_name.clone()),
            description: req.description.clone().unwrap_or_default(),
            tool_call_id,
            action_ref: Id::new("obj")?,
            options: vec![
                DecisionOption {
                    id: "allow".into(),
                    label: "Allow".into(),
                    effect: DecisionEffect::AllowOnce,
                    native_value_ref: Id::new("obj")?,
                },
                DecisionOption {
                    id: "deny".into(),
                    label: "Deny".into(),
                    effect: DecisionEffect::Deny,
                    native_value_ref: Id::new("obj")?,
                },
            ],
            requested_permissions_ref: None,
            input_digest: digest,
        }))
    };
    Ok(Interaction {
        meta: remuda_protocol::EntityMeta {
            id: interaction_id.clone(),
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        instance_id: mapper.instance_id.clone(),
        run_id: Some(mapper.run_id.clone()),
        host_id: mapper.host_id.clone(),
        kind: if ask {
            InteractionKind::Question
        } else {
            InteractionKind::Approval
        },
        request_key: InteractionRequestKey {
            native: NativeRequestKey::Rpc {
                value_type: NativeRequestValueType::String,
                value: env.request_id.clone(),
            },
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            connection_epoch: Id::new("epoch")?,
        },
        request_version: U64(1),
        state: InteractionState::Pending,
        blocking: true,
        answerable: true,
        carrier: InteractionCarrier::ClaudeControl,
        request,
        deadline: unknown("none"),
        deadline_source: remuda_protocol::DeadlineSource::None,
        answer: unknown("pending"),
        delivery: remuda_protocol::DeliveryState::NotSent,
        resolution: unknown("pending"),
    })
}

fn question_request(req: &CanUseToolRequest) -> QuestionRequest {
    let questions = req
        .input
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let fields = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let title = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("question")
                .to_owned();
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(opt_i, option)| QuestionOption {
                    id: option
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    label: option
                        .get("label")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| format!("option-{opt_i}")),
                })
                .collect();
            let multi = question
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            QuestionField {
                id: format!("q{index}"),
                title: title.clone(),
                description: question
                    .get("header")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                input: if multi {
                    QuestionInput::MultiSelect
                } else {
                    QuestionInput::SingleSelect
                },
                required: true,
                options,
                allow_free_text: false,
                sensitive: false,
            }
        })
        .collect();
    QuestionRequest {
        title: "AskUserQuestion".into(),
        fields,
    }
}

fn permission_from_answer(
    input: &Value,
    answer: &InteractionAnswer,
) -> DriverResult<ControlSuccessPayload> {
    match answer {
        InteractionAnswer::Approval(approval) => {
            if approval.option_id == "allow" {
                Ok(ControlSuccessPayload::Permission(PermissionResult::Allow {
                    updated_input: input.clone(),
                    updated_permissions: None,
                }))
            } else {
                Ok(ControlSuccessPayload::Permission(PermissionResult::Deny {
                    message: "host denied".into(),
                    interrupt: Some(false),
                }))
            }
        }
        InteractionAnswer::Question(question) => {
            let mut updated = input.clone();
            let mut answers = serde_json::Map::new();
            for (field_id, field) in &question.answers {
                let text = field
                    .text
                    .clone()
                    .or_else(|| field.option_ids.first().cloned())
                    .unwrap_or_default();
                let key = input
                    .get("questions")
                    .and_then(Value::as_array)
                    .and_then(|questions| {
                        field_id
                            .strip_prefix('q')
                            .and_then(|n| n.parse::<usize>().ok())
                            .and_then(|i| questions.get(i))
                    })
                    .and_then(|q| q.get("question"))
                    .and_then(Value::as_str)
                    .unwrap_or(field_id.as_str())
                    .to_owned();
                answers.insert(key, Value::String(text));
            }
            if let Some(obj) = updated.as_object_mut() {
                obj.insert("answers".into(), Value::Object(answers));
            }
            Ok(ControlSuccessPayload::Permission(PermissionResult::Allow {
                updated_input: updated,
                updated_permissions: None,
            }))
        }
        _ => Err(DriverError::InvalidLaunchSpec(
            "unsupported interaction answer".into(),
        )),
    }
}

fn prompt_text(blocks: &[ContentBlock]) -> DriverResult<String> {
    let mut parts = Vec::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    if parts.is_empty() {
        return Err(DriverError::InvalidLaunchSpec(
            "prompt has no text blocks".into(),
        ));
    }
    Ok(parts.join(""))
}

fn reject_bot_bypass(spec: &InstanceSpec, origin: InputOrigin) -> DriverResult<()> {
    let gated = match &spec.permission_mode {
        PermissionMode::Claude(claude) => matches!(
            claude.mode,
            ClaudePermissionMode::BypassPermissions | ClaudePermissionMode::DontAsk
        ),
        _ => false,
    };
    if gated && matches!(origin, InputOrigin::Bot | InputOrigin::Agent) {
        return Err(DriverError::BypassNotAllowedForBot);
    }
    Ok(())
}

fn policy_from_recipe(recipe: &LaunchRecipe, spec: &InstanceSpec) -> PermissionPolicy {
    if recipe.permission.cli_mode.as_deref() == Some("bypassPermissions")
        || recipe
            .permission
            .extra_flags
            .iter()
            .any(|flag| flag.contains("skip-permissions"))
    {
        return PermissionPolicy::AutoAllow;
    }
    if recipe.permission.cli_mode.as_deref() == Some("dontAsk")
        || recipe
            .technical_debt
            .iter()
            .any(|tag| tag == crate::TECH_DEBT_M0_PERM_01)
    {
        return PermissionPolicy::AutoDeny;
    }
    match &spec.permission_mode {
        PermissionMode::Claude(claude) if claude.mode == ClaudePermissionMode::DontAsk => {
            PermissionPolicy::AutoDeny
        }
        PermissionMode::Claude(claude)
            if claude.mode == ClaudePermissionMode::BypassPermissions =>
        {
            PermissionPolicy::AutoAllow
        }
        _ => PermissionPolicy::Host,
    }
}

fn apply_bypass_flag(recipe: &mut LaunchRecipe) {
    if recipe.permission.cli_mode.as_deref() != Some("bypassPermissions") {
        return;
    }
    let flag = "--allow-dangerously-skip-permissions";
    if !recipe.argv.iter().any(|token| token == flag) {
        recipe.argv.push(flag.into());
    }
    if !recipe
        .permission
        .extra_flags
        .iter()
        .any(|token| token == flag)
    {
        recipe.permission.extra_flags.push(flag.into());
    }
}

fn refuse_prohibited_argv(argv: &[String]) -> DriverResult<()> {
    if argv.iter().any(|token| {
        crate::flags::is_banned_flag_token(token)
            || matches!(
                token.as_str(),
                "--bare" | "--safe-mode" | "--no-session-persistence" | "--continue"
            )
    }) {
        return Err(DriverError::NativeFeatureDisabled(
            "refusing prohibited flag on claude-print argv".into(),
        ));
    }
    Ok(())
}

fn map_wire(error: remuda_claude_wire::Error) -> DriverError {
    match error {
        remuda_claude_wire::Error::Io(error)
        | remuda_claude_wire::Error::Spawn { source: error, .. } => DriverError::Io(error),
        remuda_claude_wire::Error::HandshakeTimeout { .. }
        | remuda_claude_wire::Error::HandshakeFailed { .. }
        | remuda_claude_wire::Error::HandshakeEof { .. }
        | remuda_claude_wire::Error::StdinClosed
        | remuda_claude_wire::Error::StdoutClosed => DriverError::ControlUnavailable,
        remuda_claude_wire::Error::ForbiddenFlag { flag } => {
            DriverError::InvalidLaunchSpec(format!("forbidden flag {flag}"))
        }
        remuda_claude_wire::Error::MissingPipe(_)
        | remuda_claude_wire::Error::LineTooLong { .. }
        | remuda_claude_wire::Error::Json(_)
        | remuda_claude_wire::Error::InvalidPermissionMode => {
            DriverError::InvalidLaunchSpec(error.to_string())
        }
    }
}

fn now() -> DriverResult<Timestamp> {
    let t = time::OffsetDateTime::now_utc();
    let text = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond(),
    );
    Timestamp::try_from(text).map_err(DriverError::Protocol)
}

fn dummy_digest() -> Digest {
    Digest::try_from(format!("sha256:{:0>64}", "0")).expect("digest")
}

fn fallback_obj() -> Id {
    Id::new("obj").expect("obj prefix")
}

fn fallback_host() -> HostId {
    "hst_01993ab0-0000-7000-8000-000000000001"
        .parse()
        .expect("host")
}

fn known_or_unknown(value: Option<&str>) -> Knowledge<String> {
    match value {
        Some(value) => Knowledge::Known {
            value: value.to_owned(),
        },
        None => unknown("not-emitted"),
    }
}

fn unknown<T>(reason: &str) -> Knowledge<T> {
    Knowledge::Unknown {
        reason: reason.into(),
        evidence_event_ids: Vec::<EventId>::new(),
    }
}

fn opt_u64(value: Option<u64>) -> Knowledge<U64> {
    match value {
        Some(value) => Knowledge::Known { value: U64(value) },
        None => unknown("not-emitted"),
    }
}

fn tool_category(name: &str) -> ToolCategory {
    match name {
        "Bash" => ToolCategory::Shell,
        "Read" => ToolCategory::FileRead,
        "Write" | "Edit" => ToolCategory::FileWrite,
        "Grep" | "Glob" => ToolCategory::Search,
        "Workflow" => ToolCategory::Workflow,
        "Task" => ToolCategory::Agent,
        other if other.starts_with("mcp__") => ToolCategory::Mcp,
        _ => ToolCategory::Other,
    }
}

fn workflow_state(status: &str) -> WorkflowState {
    match status {
        "pending" => WorkflowState::Queued,
        "running" => WorkflowState::Running,
        "completed" => WorkflowState::Completed,
        "failed" => WorkflowState::Failed,
        "killed" | "stopped" | "cancelled" => WorkflowState::Cancelled,
        _ => WorkflowState::Unknown,
    }
}

/// Mapping / gating helpers used by `tests/claude_print_review.rs`.
#[doc(hidden)]
pub mod review {
    use super::*;

    /// Map one stdout JSON object the same way the live reader does.
    pub fn map_stdout_json(value: Value) -> DriverResult<Vec<Observation>> {
        let frame = Outbound::from_value(value);
        let mut mapper = Mapper {
            ids: NativeIds::default(),
            seq: 0,
            instance_id: InstanceId::new(),
            run_id: RunId::new(),
            journal_id: fallback_obj(),
            host_id: fallback_host(),
            session_id: "review-session".into(),
            pin: BinaryPin {
                abs_path: String::new(),
                version: "review".into(),
                sha256: dummy_digest(),
            },
        };
        map_outbound(&mut mapper, &frame)
    }

    /// Serialize a host `control_response` for a permission answer.
    pub fn permission_control_json(
        request_id: &str,
        input: &Value,
        answer: &InteractionAnswer,
    ) -> DriverResult<Value> {
        let payload = permission_from_answer(input, answer)?;
        let inbound = Inbound::control_success(request_id, payload);
        Ok(serde_json::to_value(&inbound)?)
    }

    /// Driver-level bot/dontAsk/bypass gate.
    pub fn reject_bot(spec: &InstanceSpec, origin: InputOrigin) -> DriverResult<()> {
        reject_bot_bypass(spec, origin)
    }

    /// Final argv check for `--bare` / `--no-session-persistence`.
    pub fn refuse_argv(argv: &[String]) -> DriverResult<()> {
        refuse_prohibited_argv(argv)
    }
}
