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
use remuda_protocol::hubnode::AttachmentKind;
use remuda_protocol::{
    ApprovalRequest, ClaudePermissionMode, Completeness, ContentBlock, ContentStatus, Cost,
    DecisionEffect, DecisionOption, Digest, DriverInput, DriverKind, EffectiveModel,
    EffortEffective, EffortPayload, EffortSource, EventId, HostId, Id, InputAccounting,
    InputOrigin, InstanceId, InstanceSpec, Interaction, InteractionAnswer, InteractionCarrier,
    InteractionId, InteractionKind, InteractionRequest, InteractionRequestKey, InteractionState,
    Knowledge, LifecyclePayload, LifecycleTopic, MessagePayload, MessagePhase, MessageRole,
    ModelPayload, MutationOperation, NativeLifecycle, NativeRef, NativeRequestKey,
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
    /// Hub-issued instance context; separate from ordinary environment overlays.
    pub agent_mcp: Option<crate::agent_mcp::AgentMcpContext>,
    /// Override `--setting-sources`.
    pub setting_sources: Option<Vec<String>>,
    /// Initialize handshake timeout.
    pub handshake_timeout: Duration,
    /// Total bound for [`Driver::close`]. The ladder spends it in two equal
    /// slices — stdin EOF with a bounded wait, then SIGTERM to the process group
    /// with a bounded wait — before SIGKILL and a short fixed reap (§2.3).
    ///
    /// Stdin EOF is a request, not a guarantee: a child mid-turn, blocked on an
    /// unanswered `can_use_tool`, or one that stopped draining stdin, can ignore
    /// it indefinitely. An unbounded close would hang forever on the long-lived
    /// sdk carrier, so the EOF request *and* every wait are inside this bound.
    pub close_timeout: Duration,
    /// Host-validated `--settings` overlay. Contents are never logged.
    pub settings_overlay_path: Option<PathBuf>,
}

impl ClaudePrintOptions {
    /// Print driver with unknown (Agent) origin and the env/file broker.
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
            origin: InputOrigin::Agent,
            broker: Arc::new(EnvFileSecretBroker::env_only()),
            extra_env: std::collections::BTreeMap::new(),
            agent_mcp: None,
            setting_sources: None,
            handshake_timeout: Duration::from_secs(30),
            close_timeout: Duration::from_secs(10),
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

#[path = "claude_print_stream.rs"]
mod stream;

#[path = "claude_transcript_records.rs"]
mod records;

struct Mapper {
    stream: stream::StreamState,
    ids: NativeIds,
    seq: u64,
    instance_id: InstanceId,
    run_id: RunId,
    journal_id: Id,
    host_id: HostId,
    session_id: String,
    pin: BinaryPin,
    /// Driver stamped on every Observation. `claude-print` for the live reader;
    /// `shell-pty` when a promoted terminal replays the same records (D-025).
    driver_kind: DriverKind,
    /// Source channel for the same reason: `stdout` live, `transcript` on replay.
    channel: SourceChannel,
}

#[derive(Default)]
struct NativeIds {
    messages: HashMap<String, (Id, u64)>,
    tools: HashMap<String, Id>,
    workflows: HashMap<String, Id>,
    phases: HashMap<String, Id>,
    /// One result node per native tool id, keyed `tool-result:{native}`. A
    /// tool result is its own node — joined to the call via `tool_call_id`,
    /// not a mutation of the call node — so a transcript replay emits the same
    /// open→(replace) sequence the live stream does (promotion parity,
    /// D-025/D-028). The first record opens it at revision 1; a background
    /// subagent's later `<task-notification>` replaces it at revision 5, which
    /// outranks the hook relay's SubagentStop (revision 4) because the
    /// notification carries the authoritative status (killed/failed).
    tool_results: HashMap<String, (Id, u64)>,
    /// Instance id scope for [`Id::derive`], set when the mapper knows it.
    scope: Option<String>,
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
        // Deterministic on (instance scope, native tool_use_id) so the hook
        // relay's Pre/PostToolUse observations and this transcript replay fold
        // onto one node (live-view design §2.3); random ids would draw a second
        // card whenever both channels are present (promoted shell-pty).
        let id = match &self.scope {
            Some(scope) => Id::derive("obj", scope, native)?,
            None => Id::new("obj")?,
        };
        self.tools.insert(native.to_owned(), id.clone());
        Ok(id)
    }

    /// The result node for a native tool id and its opening mutation: a fresh
    /// node opened at revision 1 on first use. The call is joined via
    /// `tool_call_id`, never mutated.
    fn tool_result_open(&mut self, native: &str) -> DriverResult<(Id, U64, MutationOperation)> {
        if let Some((id, rev)) = self.tool_results.get_mut(native) {
            *rev += 1;
            return Ok((id.clone(), U64(*rev), MutationOperation::Replace));
        }
        let id = match &self.scope {
            Some(scope) => Id::derive("obj", scope, &format!("tool-result:{native}"))?,
            None => Id::derive("obj", "stdout-tool-result", native)?,
        };
        self.tool_results.insert(native.to_owned(), (id.clone(), 1));
        Ok((id, U64(1), MutationOperation::Open))
    }

    /// The result node id for a native tool id (creating an unopened entry so
    /// a completion can replace a launch result even if observed first).
    fn tool_result_id(&mut self, native: &str) -> DriverResult<Id> {
        if let Some((id, _)) = self.tool_results.get(native) {
            return Ok(id.clone());
        }
        let id = match &self.scope {
            Some(scope) => Id::derive("obj", scope, &format!("tool-result:{native}"))?,
            None => Id::derive("obj", "stdout-tool-result", native)?,
        };
        self.tool_results.insert(native.to_owned(), (id.clone(), 1));
        Ok(id)
    }

    /// Build id tables whose tool ids derive deterministically from the instance
    /// scope, matching the hook relay's `Id::derive("obj", instance, native)`
    /// (live-view design §2.3).
    fn scoped(scope: impl Into<String>) -> Self {
        Self {
            scope: Some(scope.into()),
            ..Self::default()
        }
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
    /// Whether the `exited` session lifecycle has already been emitted, so the
    /// reader task and [`Driver::close`] cannot both emit it (§2.3: exactly once).
    exit_emitted: AtomicBool,
    /// Last successfully launched spec; `resume` re-materializes from this.
    last_spec: Mutex<Option<InstanceSpec>>,
}

/// Native Claude print driver (`claude -p` stream-json).
pub struct ClaudePrintDriver {
    options: ClaudePrintOptions,
    inner: Arc<Inner>,
    reader: Mutex<Option<JoinHandle<()>>>,
    /// Carrier this object launches and stamps.
    ///
    /// `claude-print` and `claude-sdk` are the same transport (stream-json over
    /// stdio, same handshake, same mapper) differing by one launch flag, so
    /// `claude_sdk.rs` drives this engine with [`DriverKind::ClaudeSdk`] rather
    /// than forking ~1000 lines of mapper (`print-replacement.md` §2.5, batch 2).
    carrier: DriverKind,
}

impl ClaudePrintDriver {
    /// Build a driver from explicit options.
    pub fn new(options: ClaudePrintOptions) -> Self {
        Self::with_carrier(options, DriverKind::ClaudePrint)
    }

    /// Wire name of the carrier this object drives, for user-facing messages and
    /// logs. Sibling drivers pass the same kebab-case spelling.
    fn carrier_name(&self) -> &'static str {
        match self.carrier {
            DriverKind::ClaudeSdk => "claude-sdk",
            _ => "claude-print",
        }
    }

    /// [`Self::new`], stamping and launching `carrier` instead of `claude-print`.
    pub(crate) fn with_carrier(options: ClaudePrintOptions, carrier: DriverKind) -> Self {
        Self {
            options,
            inner: Arc::new(Inner {
                live: Mutex::new(None),
                mapper: Mutex::new(Mapper {
                    stream: stream::StreamState::default(),
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
                    driver_kind: carrier,
                    channel: SourceChannel::Stdout,
                }),
                policy: Mutex::new(PermissionPolicy::Host),
                events: Mutex::new(None),
                closed: AtomicBool::new(false),
                exit_emitted: AtomicBool::new(false),
                last_spec: Mutex::new(None),
            }),
            reader: Mutex::new(None),
            carrier,
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
        if spec.driver != self.carrier {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "{} driver requires driverKind {}, got {:?}",
                self.carrier_name(),
                self.carrier_name(),
                spec.driver
            )));
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
            native_home_managed: Some(!self.options.inherit_default_config),
            settings_overlay_path: self.options.settings_overlay_path.clone(),
            secret_policy: None,
        };
        let mut recipe = materialize(&request)?;
        apply_bypass_flag(&mut recipe);
        refuse_prohibited_argv(&recipe.argv)?;
        let policy = policy_from_recipe(&recipe, &spec);
        let env = self.resolve_env(&spec, &recipe).await?;
        let mut command = Command::new(&recipe.binary.abs_path);
        command.args(&recipe.argv).current_dir(&recipe.cwd);
        // Start from nothing: the Node's own environment holds the bootstrap
        // and host tokens, and inheriting it hands them to the model
        // (security-review-2 S1).
        command.env_clear();
        for (key, value) in crate::child_env::base_env() {
            command.env(key, value);
        }
        for (key, value) in &env {
            command.env(key, value);
        }
        for (key, value) in &self.options.extra_env {
            if crate::child_env::is_denied(key) {
                continue;
            }
            command.env(key, value);
        }
        if let Some(context) = &self.options.agent_mcp {
            command.envs(context.environment()?);
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
                stream: stream::StreamState::default(),
                ids: NativeIds::default(),
                seq: 0,
                instance_id: instance_id.clone(),
                run_id,
                journal_id,
                host_id: spec.host.clone(),
                session_id: session_id.clone(),
                pin: recipe.binary.clone(),
                driver_kind: self.carrier,
                channel: SourceChannel::Stdout,
            };
        }
        *self.inner.policy.lock().await = policy;
        *self.inner.events.lock().await = Some(tx);
        *self.inner.last_spec.lock().await = Some(spec.clone());
        self.inner.closed.store(false, Ordering::SeqCst);
        self.inner.exit_emitted.store(false, Ordering::SeqCst);

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
        let carrier = self.carrier_name();
        let reader = tokio::spawn(async move {
            map_loop(inner, outbound, carrier).await;
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
            // The materializer already refuses these, but this is the last
            // gate before the value reaches a process (security-review-2 S2).
            // Capability entries are driver-computed grants (D-045): the
            // REMUDA_ deny prefix guards spec- and host-supplied names, not
            // values the driver itself attaches only after the grant gates.
            // The hole is narrow to the granted-handshake name only: any other
            // denied (REMUDA_-prefixed) name is refused even when tagged
            // Capability. The value rides the entry from the grant.
            if crate::child_env::is_denied(&entry.name)
                && entry.name != crate::launch::skills::CAPABILITY_COMPUTER_USE_ENV
            {
                debug!(name = %entry.name, "refusing a denied env name");
                continue;
            }
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
                EnvAllowlistSource::Capability => {
                    if let Some(value) = entry.secret_ref.as_deref() {
                        env.insert(entry.name.clone(), value.to_owned());
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
        Ok(capability_snapshot(self.carrier, &pin, U64(1), U64(1))?)
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
                live.process
                    .send_user(prompt_content(&prompt.blocks)?)
                    .await
                    .map_err(map_wire)?;
                Ok(DriverAck::transport_written())
            }
            DriverInput::Steer(_) => Err(DriverError::CapabilityUnknown("steer".into())),
            DriverInput::ModelSwitch(switch) => {
                // §9.1: stream-json has no in-session effort channel. A
                // `set_settings` control request is silently ignored by
                // claude -p (measured 2.1.221, see evidence/effort-sync-1.md),
                // so accepting an effort switch here would be a lie — the
                // level only changes via `--effort` at launch on this carrier.
                if switch
                    .effort
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                {
                    return Err(DriverError::CapabilityUnsupported(format!(
                        "{} cannot switch effort in-session: relaunch with --effort; \
                         use the claude-pty or shell-pty carrier for /effort",
                        self.carrier_name()
                    )));
                }
                if !switch.model_id.is_empty() {
                    live.process
                        .set_model(Some(switch.model_id.clone()))
                        .await
                        .map_err(map_wire)?;
                }
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
        let recipe = live_guard.as_ref().map(|live| live.recipe.clone());
        if let Some(live) = live_guard.as_mut() {
            close_ladder(live, self.options.close_timeout, self.carrier_name()).await;
        }
        *live_guard = None;
        drop(live_guard);

        // Stop this launch's reader before touching the events channel. The
        // child is already reaped, so its stdout is at EOF and `map_loop` is on
        // its way out; give it a short bound to finish and deliver the `exited`
        // lifecycle itself, then abort it if it is still parked.
        //
        // The abort is the point. A reader outlives close detached, and a later
        // `start()` on this driver resets `exit_emitted` for the new launch; a
        // reader A still parked on `inner.live` at that moment could then emit a
        // *second* `exited` into launch B's channel. Joining it here (or killing
        // it) closes that window: by the time `close` returns, no task from this
        // launch can emit.
        if let Some(mut reader) = self.reader.lock().await.take()
            && tokio::time::timeout(Duration::from_millis(500), &mut reader)
                .await
                .is_err()
        {
            reader.abort();
        }

        // Whichever path got there first — the reader finishing naturally, or
        // this call covering an aborted one — emits exactly once
        // (`exit_emitted`), and only while `events` is still live.
        let _ = emit_exit(&self.inner, "exited").await;
        *self.inner.events.lock().await = None;
        // S5: the child has exited, so the launch overlays can go. The label is
        // the carrier's wire name, matching the kebab-case the sibling drivers
        // pass (`claude-pty`, `claude-bg`, `generic-pty`).
        let carrier = self.carrier_name();
        if let Some(recipe) = recipe {
            crate::recipe::report_launch_cleanup(&recipe, carrier);
        }
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
        spec.driver = self.carrier;
        spec.kind = remuda_protocol::AgentKind::Claude;
        self.launch(spec, SessionAction::Resume { session_id })
            .await
    }

    async fn start_resumed(
        &self,
        mut spec: InstanceSpec,
        session_id: String,
    ) -> DriverResult<RunHandle> {
        if session_id.trim().is_empty() {
            return Err(DriverError::NativeSessionNotFound);
        }
        spec.driver = self.carrier;
        self.launch(spec, SessionAction::Resume { session_id })
            .await
    }
}

/// Close the child on a bounded ladder: stdin EOF with a bounded wait, SIGTERM
/// to the process group with a bounded wait, then SIGKILL to the group plus a
/// bounded reap (`print-replacement.md` §2.3).
///
/// Stdin EOF is only a request. A child mid-turn, blocked on a `can_use_tool`
/// nobody answered, or one that simply stopped draining stdin, can ignore it
/// for as long as it likes — and on the long-lived sdk carrier the previous
/// unbounded `wait()` turned that into an `instance.close` that never returned.
/// **Every** rung is bounded, including the EOF request itself: when the child
/// stops reading and the 64-slot writer channel fills, enqueuing the EOF blocks
/// exactly the way the subsequent wait could. The function never returns while
/// still holding a live child it could kill.
async fn close_ladder(live: &mut Live, timeout: Duration, carrier: &str) {
    // The group id equals the direct child's pid because `spawn_command` puts it
    // in its own group. Read it before any wait: once reaped, `id()` is `None`.
    let pgid = live.process.id().and_then(|pid| i32::try_from(pid).ok());

    // Rung 1: the graceful path. Request stdin EOF, then give the child the
    // whole first slice to leave. The two steps share one bound: `close_stdin`
    // enqueues onto the writer channel, and that enqueue can block when the
    // child stopped draining and the channel is full — so it must sit inside
    // the timeout, not before it. Most closes end here.
    let first_slice = timeout.mul_f32(0.5);
    let request_eof_and_wait = async {
        let _ = live.process.close_stdin().await;
        let _ = live.process.wait().await;
    };
    if tokio::time::timeout(first_slice, request_eof_and_wait)
        .await
        .is_ok()
    {
        return;
    }

    // Rung 2: SIGTERM to the whole group, then the second slice. The native
    // `interrupt` control request is not used here: it travels over stdin, which
    // rung 1 already asked to close, so nothing further can be written — and
    // interrupting before EOF would cancel in-flight work on every ordinary
    // close, which `instance.close` does not promise. The interrupt path stays
    // in `Driver::cancel`.
    //
    // The group gets the signal so a `Bash` tool or MCP server the child spawned
    // leaves with it instead of outliving the instance. A child with a handler
    // can use this slice to flush its transcript.
    debug!(carrier, "close: stdin EOF ignored, terminating the group");
    terminate_group(pgid, carrier);
    let second_slice = timeout.saturating_sub(first_slice);
    if tokio::time::timeout(second_slice, live.process.wait())
        .await
        .is_ok()
    {
        return;
    }

    // Rung 3: SIGKILL to the group, which nothing can ignore, then reap so a
    // zombie cannot survive close. `kill` reaches only the direct child, hence
    // the group signal first. The reap has a short bound for a child wedged in
    // uninterruptible kernel IO; such a process cannot be killed by userspace,
    // but close still returns rather than hanging.
    warn!(carrier, "close: SIGTERM ignored, killing the process group");
    kill_group(pgid, carrier);
    let _ = live.process.kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), live.process.wait()).await;
}

/// SIGTERM every member of the child's process group. Unix only; on other
/// platforms the child has no group to signal and `kill()` in the ladder is the
/// fallback, so this is a no-op there. `None` means the pid was already gone.
#[cfg(unix)]
fn terminate_group(pgid: Option<i32>, carrier: &str) {
    if let Some(pgid) = pgid {
        signal_child_group(pgid, nix::sys::signal::Signal::SIGTERM, carrier);
    }
}

#[cfg(not(unix))]
fn terminate_group(pgid: Option<i32>, carrier: &str) {
    let _ = (pgid, carrier);
}

/// SIGKILL every member of the child's process group. Unix only.
#[cfg(unix)]
fn kill_group(pgid: Option<i32>, carrier: &str) {
    if let Some(pgid) = pgid {
        signal_child_group(pgid, nix::sys::signal::Signal::SIGKILL, carrier);
    }
}

#[cfg(not(unix))]
fn kill_group(pgid: Option<i32>, carrier: &str) {
    let _ = (pgid, carrier);
}

/// Send a signal to every member of process group `pgid`.
///
/// `unsafe` is forbidden workspace-wide, so this goes through `nix` rather than
/// a raw `libc::killpg` — the same path `shell_pty/lifecycle.rs` uses. A group
/// that is already gone (`ESRCH`) is the outcome being asked for, and `EPERM`
/// means something in it is not ours to signal; neither is worth failing a close
/// over, so both are logged and swallowed. The `nix` signal type is kept out of
/// the non-unix callers' signatures so the `cfg(not(unix))` stubs compile.
#[cfg(unix)]
fn signal_child_group(pgid: i32, signal: nix::sys::signal::Signal, carrier: &str) {
    if pgid <= 0 {
        return;
    }
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(errno) => debug!(carrier, pgid, %errno, "process-group signal failed"),
    }
}

async fn map_loop(
    inner: Arc<Inner>,
    mut outbound: mpsc::Receiver<Outbound>,
    carrier: &'static str,
) {
    while let Some(frame) = outbound.recv().await {
        if let Err(error) = handle_frame(&inner, frame).await {
            warn!(carrier, %error, "stream-json map failed");
        }
    }
    if let Err(error) = emit_exit(&inner, "exited").await {
        debug!(carrier, %error, "exit lifecycle");
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

/// Emit the session `exited` lifecycle, at most once per launch.
///
/// Two callers race for it: the reader task when stdout hits EOF, and
/// [`Driver::close`] after the ladder. Whichever arrives first wins; the other
/// is a no-op, so a consumer never sees the session exit twice (§2.3).
async fn emit_exit(inner: &Inner, status: &str) -> DriverResult<()> {
    if inner.exit_emitted.swap(true, Ordering::SeqCst) {
        return Ok(());
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
        Outbound::Assistant(msg) => stream::map_assistant(mapper, msg),
        Outbound::User(msg) => map_user(mapper, msg),
        Outbound::Result(result) => map_result(mapper, result),
        Outbound::StreamEvent(event) => stream::map_stream(mapper, event),
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

/// One `user` message observation carrying `text`.
fn user_text_message(
    mapper: &mut Mapper,
    uuid: Option<&str>,
    text: String,
) -> DriverResult<Observation> {
    let key = uuid.map(ToOwned::to_owned).unwrap_or_else(|| "user".into());
    let (id, rev, op) = mapper.ids.message(&key)?;
    mapper.observation(
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
            blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
            target_block: None,
            parent_tool_call_id: None,
            native_origin: known_or_unknown(uuid),
            // Overwritten by the transcript mapper, which has the evidence to
            // classify. stdout `user` frames are the prompt we just sent.
            origin: Some(remuda_protocol::MessageOrigin::Human),
            command_id: None,
            prompt_mode: None,
            status: ContentStatus::Complete,
        })),
    )
}

fn map_user(mapper: &mut Mapper, msg: &UserMessage) -> DriverResult<Vec<Observation>> {
    let mut out = Vec::new();
    match &msg.message.content {
        UserContent::Text(text) => {
            // An injected task-notification can arrive as a bare string too;
            // fold it before the human-shaped copy is rendered.
            out.extend(task_notification_results(mapper, text)?);
            out.push(user_text_message(
                mapper,
                msg.uuid.as_deref(),
                text.clone(),
            )?);
        }
        UserContent::Blocks(blocks) => {
            // A user array is usually tool results, but Claude also records
            // plain text there — interrupt notices most visibly. Dropping those
            // loses the reason a turn stopped.
            let mut texts = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            texts.push(text.to_owned());
                        }
                    }
                    Some("tool_result") => {
                        let tool_use_id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        // The result occupies its own node (opened here), joined
                        // to the call via `tool_call_id` — same sequence as the
                        // live stream, so promoting a terminal from this
                        // transcript is indistinguishable from streaming it.
                        let call_id = mapper.ids.tool(tool_use_id)?;
                        let (result_id, revision, operation) =
                            mapper.ids.tool_result_open(tool_use_id)?;
                        let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
                        let text = match block.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };
                        // A backgrounded (or build-deferred) subagent's launch
                        // tool_result is written *immediately* with
                        // `toolUseResult.isAsync`; the real completion arrives
                        // later as an injected `<task-notification>` user
                        // record. Mark the launch `Partial` so the task row
                        // stays running (and the web shows "running in
                        // background"); the notification replaces it for real.
                        let async_launch = remuda_protocol::tool_result_is_async_launch(
                            msg.tool_use_result.as_ref(),
                        );
                        out.push(mapper.observation(
                            Completeness::Structured,
                            NativeRequestKey::None,
                            ObservationPayload::ToolResult(Box::new(ToolResultPayload {
                                mutation: NodeMutation {
                                    node_id: result_id,
                                    revision,
                                    operation,
                                    base_revision: (revision.0 > 1).then_some(U64(revision.0 - 1)),
                                },
                                tool_call_id: call_id,
                                stage: if async_launch && !is_error {
                                    ResultStage::Partial
                                } else {
                                    ResultStage::Final
                                },
                                outcome: if is_error {
                                    ToolOutcome::Failed
                                } else {
                                    ToolOutcome::Succeeded
                                },
                                blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
                                structured_result: match msg.tool_use_result.clone() {
                                    Some(value) => Knowledge::Known { value },
                                    None => Knowledge::NotApplicable,
                                },
                                exit_code: Knowledge::NotApplicable,
                                changes: Vec::new(),
                            })),
                        )?);
                    }
                    _ => {}
                }
            }
            // Task-notification text rides alongside tool results in the same
            // user record on some builds; fold every one before emitting the
            // joined human-readable text message.
            for text in &texts {
                out.extend(task_notification_results(mapper, text)?);
            }
            if !texts.is_empty() {
                out.push(user_text_message(
                    mapper,
                    msg.uuid.as_deref(),
                    texts.join("\n"),
                )?);
            }
        }
    }
    Ok(out)
}

/// Turn an injected `<task-notification>` text block into the final
/// ToolResult that closes the background subagent's task row.
///
/// Returns zero observations when the text is not a notification or carries no
/// `<tool-use-id>` (a background shell command with no tool row to fold onto).
/// The message itself is still rendered by the caller.
fn task_notification_results(mapper: &mut Mapper, text: &str) -> DriverResult<Vec<Observation>> {
    let Some(note) = remuda_protocol::TaskNotification::parse(text) else {
        return Ok(Vec::new());
    };
    let Some(native_tool_id) = note.tool_use_id.clone().filter(|id| !id.is_empty()) else {
        return Ok(Vec::new());
    };
    let call_id = mapper.ids.tool(&native_tool_id)?;
    let result_id = mapper.ids.tool_result_id(&native_tool_id)?;
    // Fixed revision 5 on the result node: launch Partial opens at 1, the hook
    // SubagentStop replaces at 4, and this authoritative notification (it
    // carries the real status, including killed/failed) replaces at 5 — so it
    // always wins.
    let revision = U64(5);
    let outcome = note.outcome();
    let status = note.status.clone();
    let task_id = note.task_id.clone();
    let summary = note.summary.clone();
    let body = note
        .result
        .clone()
        .or_else(|| note.summary.clone())
        .unwrap_or_default();
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::ToolResult(Box::new(ToolResultPayload {
            mutation: NodeMutation {
                node_id: result_id,
                revision,
                operation: MutationOperation::Replace,
                base_revision: Some(U64(4)),
            },
            tool_call_id: call_id,
            stage: ResultStage::Final,
            outcome,
            blocks: if body.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(Box::new(TextBlock { text: body }))]
            },
            structured_result: Knowledge::Known {
                value: serde_json::json!({
                    "taskId": task_id,
                    "status": status,
                    "summary": summary,
                }),
            },
            exit_code: Knowledge::NotApplicable,
            changes: Vec::new(),
        })),
    )?])
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
            name: None,
            description: None,
            totals: None,
            launched_at: None,
            live: None,
            note: None,
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
            name: None,
            description: None,
            totals: None,
            launched_at: None,
            live: None,
            note: None,
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
            name: None,
            description: None,
            totals: None,
            launched_at: None,
            live: None,
            note: None,
            result_ref: None,
        })),
    )?])
}

impl Mapper {
    /// Completeness for a streamed content block: `Partial` while it is still
    /// open, `Structured` once the final block closes it.
    ///
    /// D-028a item 3 / `print-replacement.md` §2.5: on the sdk carrier a stream
    /// delta is explicitly a partial observation and the final assistant block
    /// is the authority. `claude-print` kept every content observation
    /// `Structured` before this parameter existed, and stays that way — its
    /// consumers and journal fixtures were built against that, and nothing
    /// measured on print changed here.
    fn content_completeness(&self, closed: bool) -> Completeness {
        if closed || self.driver_kind != DriverKind::ClaudeSdk {
            Completeness::Structured
        } else {
            Completeness::Partial
        }
    }

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
                driver_kind: self.driver_kind,
                driver_version: self.pin.version.clone(),
                adapter_version: ADAPTER_VERSION.into(),
                channel: self.channel,
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
                    description: option
                        .get("description")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
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

/// Largest total of inlined image bytes on one turn.
///
/// The CLI re-compresses above its own internal budget, and a very large
/// payload is more likely to be refused than answered. Past this the driver
/// mentions paths instead, which the Read tool can still act on.
const MAX_INLINE_IMAGE_BYTES: usize = 3_584 * 1024;

/// Build the `user` content for one prompt.
///
/// With no attachments this is the plain string it has always been. With
/// attachments it becomes Anthropic content blocks: each image inlined as
/// base64, then the text. Non-image files (D-027b) are never inlined — the
/// CLI has no file block verified to work — so their `[File #n] … saved at …`
/// lines are prepended to the text block, which the harness's Read tool acts
/// on.
///
/// Base64 is not a preference. The design verified that a `source.type` of
/// `file` or a bare path is silently downgraded to text by the CLI, so the
/// image would be dropped without an error — worse than not sending it.
fn prompt_content(blocks: &[ContentBlock]) -> DriverResult<UserContent> {
    let attachments = crate::attachment::attachments_of(blocks);
    if attachments.is_empty() {
        return Ok(UserContent::Text(prompt_text(blocks)?));
    }
    let images: Vec<_> = attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Image)
        .collect();
    // Any non-image attachment rides a text block instead of an image block.
    let has_files = attachments.len() != images.len();
    let mut inline = Vec::with_capacity(images.len());
    let mut total = 0usize;
    for attachment in &images {
        let bytes = attachment.read()?;
        total = total.saturating_add(bytes.len());
        if total > MAX_INLINE_IMAGE_BYTES {
            // Too big to inline: fall back to the path mention so the agent
            // can still reach the image through its Read tool.
            tracing::warn!(
                object_id = %attachment.object_id,
                total,
                "attachments exceed the inline budget; sending paths instead"
            );
            return Ok(UserContent::Text(
                crate::attachment::text_with_path_mentions(blocks)?,
            ));
        }
        inline.push(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": attachment.media_type,
                "data": base64_of(&bytes),
            },
        }));
    }
    // File-reference lines precede the user text; when every attachment is an
    // image the text block is the prompt unchanged.
    let text = if has_files {
        crate::attachment::with_file_lines(&crate::attachment::text_of(blocks), &attachments)
    } else {
        crate::attachment::text_of(blocks)
    };
    if !text.is_empty() {
        inline.push(serde_json::json!({"type": "text", "text": text}));
    }
    Ok(UserContent::Blocks(inline))
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
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
            "refusing prohibited flag on stream-json argv".into(),
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
        // 2.1.x renamed Task to Agent; both spellings must classify as Agent so
        // the structured view's task track collects the row.
        "Task" | "Agent" => ToolCategory::Agent,
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

/// Maps Claude transcript (`.jsonl`) records into Observations.
///
/// The transcript on disk holds the same `user` / `assistant` records the
/// stream-json stdout carries, so a promoted `shell-pty` terminal (D-025) can
/// hydrate a structured view through exactly this mapper rather than a second
/// parser.
///
/// Three D-028 §7 behaviours live here on top of that replay:
///
/// - **Regrouping.** One record holds one content block and 2–7 records share a
///   `message.id`; they are buffered by [`records::GroupKey`] and replayed as
///   one message once the run is superseded. Feeding them individually is what
///   slid the tool bookkeeping by one.
/// - **Authorship.** Every `user` record is classified into a
///   [`MessageOrigin`], so injected skill bodies and hook context stop
///   masquerading as the human's words.
/// - **Retention.** `queue-operation`, `permission-mode` and
///   `attachment.queued_command` records are journaled as lifecycle rather
///   than dropped: they are the queue ledger and the mode-drift record.
///
/// §9.1 effort read-back is resolved from the `/effort` command's
/// `<local-command-stdout>` verdict (see
/// `remuda_protocol::parse_effort_stdout`), not from the next assistant turn.
///
/// Extract the text inside `<local-command-stdout>…</local-command-stdout>`.
fn extract_local_stdout(content: &str) -> String {
    let Some(start) = content.find("<local-command-stdout>") else {
        return content.trim().to_owned();
    };
    let body = &content[start + "<local-command-stdout>".len()..];
    let end = body.find("</local-command-stdout>").unwrap_or(body.len());
    body[..end].trim().to_owned()
}

/// Test-only stdout mapper over the real stream assembler.
///
/// The live path is [`ClaudePrintDriver`]'s reader task, whose [`Mapper`] is
/// private. `tests/claude_sdk_stream.rs` replays recorded NDJSON through the
/// same `map_outbound` the driver uses, so a fixture assertion is an assertion
/// about production behaviour rather than a reimplementation of it.
#[cfg(any(test, feature = "test-stub"))]
pub struct StdoutMapper {
    mapper: Mapper,
}

#[cfg(any(test, feature = "test-stub"))]
impl StdoutMapper {
    /// Mapper stamping `driver` on channel `stdout`.
    #[must_use]
    pub fn new(driver: DriverKind, session_id: &str) -> Self {
        Self {
            mapper: Mapper {
                stream: stream::StreamState::default(),
                ids: NativeIds::default(),
                seq: 0,
                instance_id: InstanceId::new(),
                run_id: RunId::new(),
                journal_id: fallback_obj(),
                host_id: fallback_host(),
                session_id: session_id.to_owned(),
                pin: BinaryPin {
                    abs_path: String::new(),
                    version: "fixture".into(),
                    sha256: dummy_digest(),
                },
                driver_kind: driver,
                channel: SourceChannel::Stdout,
            },
        }
    }

    /// Map one decoded stdout frame, exactly as the reader task does.
    pub fn map(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
        map_outbound(&mut self.mapper, &Outbound::from_value(value))
    }

    /// Native session id the mapper has adopted from `system/init`.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.mapper.session_id
    }
}

/// Transcript-to-observations mapper for a live claude-pty session.
pub struct TranscriptMapper {
    mapper: Mapper,
    group: records::Group,
    /// `(promptId, text)` of human prompts already emitted, for dedup.
    seen_prompts: std::collections::HashSet<(String, String)>,
    /// Record uuids already mapped, so a re-read tail cannot double-emit.
    seen_uuids: std::collections::HashSet<String>,
    /// §9.1 effort read-back state (dedup + source attribution).
    effort: remuda_protocol::EffortTracker,
    /// Rendezvous for Remuda-initiated switches awaiting read-back.
    effort_bridge: Option<Arc<crate::effort::EffortBridge>>,
    /// Generation the tracker is currently armed with.
    effort_generation: Option<u64>,
    /// §9.1 effective-model read-back state.
    model: remuda_protocol::ModelTracker,
    /// Rendezvous for Remuda-initiated `/model` switches.
    model_bridge: Option<Arc<crate::model::ModelBridge>>,
    /// Generation the model tracker is armed with.
    model_generation: Option<u64>,
    /// Discovered catalog stamped onto the first model observation.
    model_catalog: Option<remuda_protocol::ModelCatalogInfo>,
    /// Whether the launch model snapshot was already emitted.
    model_launch_emitted: bool,
    /// Effective permission-mode read-back state (dedup + attribution).
    permission: remuda_protocol::LivePermissionTracker,
    /// Rendezvous for Remuda-initiated permission switches.
    permission_bridge: Option<Arc<crate::permission::PermissionBridge>>,
}

impl TranscriptMapper {
    /// Build a mapper that stamps Observations for `instance_id` on `driver`.
    pub fn new(
        driver: DriverKind,
        instance_id: InstanceId,
        run_id: RunId,
        journal_id: Id,
        host_id: HostId,
        session_id: String,
        binary_version: String,
    ) -> Self {
        Self {
            mapper: Mapper {
                stream: stream::StreamState::default(),
                ids: NativeIds::scoped(instance_id.as_id().as_str()),
                seq: 0,
                instance_id,
                run_id,
                journal_id,
                host_id,
                session_id,
                pin: BinaryPin {
                    abs_path: String::new(),
                    version: binary_version,
                    sha256: dummy_digest(),
                },
                driver_kind: driver,
                channel: SourceChannel::Transcript,
            },
            group: records::Group::default(),
            seen_prompts: std::collections::HashSet::new(),
            seen_uuids: std::collections::HashSet::new(),
            effort: remuda_protocol::EffortTracker::new(),
            effort_bridge: None,
            effort_generation: None,
            model: remuda_protocol::ModelTracker::new(),
            model_bridge: None,
            model_generation: None,
            model_catalog: None,
            model_launch_emitted: false,
            permission: remuda_protocol::LivePermissionTracker::new(),
            permission_bridge: None,
        }
    }

    /// Attach the §9.1 effort bridge so this mapper drives switch read-back and
    /// emits `effort` observations. `launch` names the `--effort` selection the
    /// process started with, when there was one.
    pub(crate) fn with_effort_bridge(
        mut self,
        bridge: Arc<crate::effort::EffortBridge>,
        launch: Option<remuda_protocol::EffortSelection>,
    ) -> Self {
        if launch.is_some() {
            self.effort.mark_launch();
        }
        self.effort_bridge = Some(bridge);
        self
    }

    /// Attach the §9.1 model bridge so this mapper drives `/model` read-back
    /// and emits `model` observations. `launch` is the `--model` selection the
    /// process started with; `catalog` is the driver-resolved list the picker
    /// must show, stamped onto the first model observation.
    pub(crate) fn with_model_bridge(
        mut self,
        bridge: Arc<crate::model::ModelBridge>,
        launch: Option<String>,
        catalog: Option<remuda_protocol::ModelCatalogInfo>,
    ) -> Self {
        if launch.is_some() {
            self.model.mark_launch();
            if let Some(id) = &launch {
                bridge.note_launch_request(id.clone());
            }
        }
        if let Some(catalog) = &catalog {
            bridge.set_own_catalog(crate::model_discovery::own_ids(catalog));
        }
        self.model_bridge = Some(bridge);
        self.model_catalog = catalog;
        self
    }

    /// Emit the launch-time model baseline carrying the discovered catalog, so
    /// the picker has the real list before the first prompt. Also preloads the
    /// tracker so the first assistant record with the same id stays deduped.
    pub(crate) fn take_launch_model_snapshot(
        &mut self,
        launch: Option<&str>,
    ) -> DriverResult<Vec<Observation>> {
        let Some(id) = launch.filter(|id| !id.is_empty()) else {
            return Ok(Vec::new());
        };
        if self.model_launch_emitted {
            return Ok(Vec::new());
        }
        self.model_launch_emitted = true;
        // Seed dedup: the same id on the first assistant record is not an edge.
        self.model.observe(Some(id));
        self.model.mark_launch();
        let catalog = self.model_catalog.take();
        let payload = ObservationPayload::Model(Box::new(ModelPayload {
            requested: Some(id.to_owned()),
            effective: EffectiveModel {
                id: id.to_owned(),
                source: EffortSource::Launch,
                observed_at: now()?,
            },
            raw: None,
            catalog,
            selection_path: None,
        }));
        Ok(vec![self.mapper.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            payload,
        )?])
    }

    /// Attach the permission bridge so shift+tab switches get transcript
    /// corroboration and `permission` observations are emitted. `launch` names
    /// the mode the process started with, when known.
    pub(crate) fn with_permission_bridge(
        mut self,
        bridge: Arc<crate::permission::PermissionBridge>,
        launch: Option<ClaudePermissionMode>,
    ) -> Self {
        if let Some(mode) = launch {
            self.permission.mark_launch(mode);
            bridge.note_launch_mode(mode);
        }
        self.permission_bridge = Some(bridge);
        self
    }

    /// Map one transcript line. Blank lines and undecodable JSON yield nothing:
    /// a partially written tail is normal while the file is being appended to.
    pub fn map_line(&mut self, line: &str) -> DriverResult<Vec<Observation>> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(Vec::new());
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Ok(Vec::new());
        };
        self.map_record(value)
    }

    /// Flush whatever assistant run is still buffered.
    ///
    /// A run is normally closed by the record that supersedes it, so the last
    /// message of a transcript would otherwise sit in the buffer forever. The
    /// tailer calls this when it reaches the end of the available input.
    pub fn flush(&mut self) -> DriverResult<Vec<Observation>> {
        self.flush_group()
    }

    /// Emit the buffered assistant run, if there is one.
    fn flush_group(&mut self) -> DriverResult<Vec<Observation>> {
        let Some(record) = self.group.flush() else {
            return Ok(Vec::new());
        };
        self.emit_conversation(record)
    }

    /// Hand one already-assembled record to the stdout mapper.
    fn emit_conversation(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
        let Some(message) = value.get("message").cloned() else {
            return Ok(Vec::new());
        };
        let mut frame = serde_json::Map::new();
        frame.insert("type".into(), value["type"].clone());
        frame.insert("message".into(), message);
        for key in ["uuid", "parentToolUseId", "sessionId"] {
            if let Some(found) = value.get(key) {
                let wire = if key == "sessionId" {
                    "session_id"
                } else if key == "parentToolUseId" {
                    "parent_tool_use_id"
                } else {
                    key
                };
                frame.insert(wire.into(), found.clone());
            }
        }
        // The launch sidecar (`isAsync` / `async_launched` / `agentId`) is what
        // distinguishes an immediate background-launch tool_result from the
        // subagent's real completion; the wire frame spells it `tool_use_result`.
        if let Some(sidecar) = value.get("toolUseResult") {
            frame.insert("tool_use_result".into(), sidecar.clone());
        }
        // The link from a result back to its call. `parentToolUseId` — what this
        // mapper used to read — does not occur in a transcript at all (0 hits);
        // `sourceToolUseID` is the key Claude actually writes.
        if let Some(source) = value.get("sourceToolUseID").and_then(Value::as_str) {
            frame.insert("parent_tool_use_id".into(), Value::String(source.into()));
        }
        let mut out = map_outbound(
            &mut self.mapper,
            &Outbound::from_value(Value::Object(frame)),
        )?;
        // Stamp authorship on the messages this record produced. Tool results
        // and tool calls carry their own identity and are left alone.
        if value.get("type").and_then(Value::as_str) == Some("user") {
            let origin = records::classify_user_record(&value, &value["message"]);
            for observation in &mut out {
                if let ObservationPayload::Message(payload) = &mut observation.body
                    && payload.role == MessageRole::User
                {
                    payload.origin = Some(origin);
                }
            }
        }
        Ok(out)
    }

    /// Map one decoded transcript record.
    pub fn map_record(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
        // A sidechain record belongs to a sub-agent's own transcript view; the
        // main conversation is what the 结构 tab renders.
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return Ok(Vec::new());
        }
        if let Some(session) = value.get("sessionId").and_then(Value::as_str)
            && !session.is_empty()
        {
            self.mapper.session_id = session.to_owned();
        }
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let mut extra = Vec::new();
        if kind == "assistant" {
            extra = self.map_effort_assistant(&value)?;
            extra.extend(self.map_model_assistant(&value)?);
        } else if kind == "user" {
            extra = self.note_effort_user(&value)?;
            extra.extend(self.note_model_user(&value)?);
            extra.extend(self.note_permission_user(&value)?);
        } else if kind == "system" {
            extra = self.note_model_system(&value)?;
        } else if kind == "attachment" {
            extra = self.note_effort_attachment(&value)?;
        }
        let mut mapped = match kind {
            "assistant" => self.map_assistant_record(value),
            "user" => {
                // Any user record ends the assistant run before it.
                let mut out = self.flush_group()?;
                out.extend(self.map_user_record(value)?);
                Ok(out)
            }
            // §7: the queue ledger and mode drift are not conversation content,
            // but dropping them loses the evidence for what the composer showed.
            "queue-operation" => self.map_queue_operation(&value),
            "permission-mode" | "mode" => self.map_permission_mode(&value),
            "attachment" => self.map_attachment(&value),
            _ => Ok(Vec::new()),
        }?;
        mapped.splice(0..0, extra);
        Ok(mapped)
    }

    /// §9.1: the `/effort` command writes TWO user records — the slash markup
    /// and its `<local-command-stdout>` verdict. The slash record only arms
    /// attribution (on 2.1.272 it lands even for a dismissed dialog); the
    /// verdict settles the level and resolves or rejects the switch bridge.
    /// Returns an effort edge observation when the verdict changes the level.
    fn note_effort_user(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let Some(message) = value.get("message") else {
            return Ok(Vec::new());
        };
        let text = records::record_text(message);
        if text.contains("<command-name>/effort</command-name>") {
            let Some(word) = remuda_protocol::slash_effort_word(&text) else {
                return Ok(Vec::new());
            };
            let from_remuda = self
                .effort_bridge
                .as_ref()
                .and_then(|bridge| bridge.pending())
                .is_some_and(|request| request.command_word() == word);
            if from_remuda
                && let Some(bridge) = &self.effort_bridge
                && let Some((generation, request)) = bridge.pending_with_gen()
            {
                self.effort_generation = Some(generation);
                self.effort.arm_awaiting(request.observed_name());
            }
            self.effort.note_slash(&word, from_remuda);
            return Ok(Vec::new());
        }
        if text.contains("<local-command-stdout>") {
            let stdout = extract_local_stdout(&text);
            let verdict = remuda_protocol::parse_effort_stdout(&stdout);
            let from_remuda = self
                .effort_bridge
                .as_ref()
                .and_then(|bridge| bridge.pending())
                .is_some();
            if matches!(verdict, remuda_protocol::EffortStdout::Accepted(_)) {
                if let Some((observed, source)) = self.effort.note_stdout(&stdout, from_remuda) {
                    if let Some(generation) = self.effort_generation.take()
                        && let Some(bridge) = &self.effort_bridge
                    {
                        bridge.resolve(generation, observed);
                    }
                    return self.effort_observation(observed, source, Some(stdout));
                }
                // Non-edge accept: still resolve the switch (a switch to the
                // level already in effect is accepted), carrying the flag.
                if let Some(generation) = self.effort_generation.take()
                    && let Some(bridge) = &self.effort_bridge
                    && let remuda_protocol::EffortStdout::Accepted(observed) = verdict
                {
                    bridge.resolve(generation, observed);
                }
            } else if matches!(
                verdict,
                remuda_protocol::EffortStdout::Kept | remuda_protocol::EffortStdout::Invalid
            ) && let Some(generation) = self.effort_generation.take()
                && let Some(bridge) = &self.effort_bridge
            {
                let reason = if verdict == remuda_protocol::EffortStdout::Kept {
                    "dialog-kept"
                } else {
                    "invalid-argument"
                };
                bridge.reject(generation, reason);
            }
        }
        Ok(Vec::new())
    }

    /// A `/plan` command switches mode natively; arm slash attribution so the
    /// following `permission-mode` edge is attributed to the terminal.
    fn note_permission_user(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let Some(message) = value.get("message") else {
            return Ok(Vec::new());
        };
        let text = records::record_text(message);
        if text.contains("<command-name>/plan</command-name>") {
            self.permission.note_slash();
        }
        Ok(Vec::new())
    }

    /// §9.1: 2.1.272 rides an `ultra_effort_enter|exit` attachment on the next
    /// prompt; it corroborates the stdout verdict's ultracode flag.
    fn note_effort_attachment(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        match value.pointer("/attachment/type").and_then(Value::as_str) {
            Some("ultra_effort_enter") => {
                if let Some((observed, source)) = self.effort.note_ultra_attachment(true) {
                    return self.effort_observation(observed, source, None);
                }
            }
            Some("ultra_effort_exit") => {
                if let Some((observed, source)) = self.effort.note_ultra_attachment(false) {
                    return self.effort_observation(observed, source, None);
                }
            }
            _ => {}
        }
        Ok(Vec::new())
    }

    /// Build the §9.1 effort observation for one edge.
    fn effort_observation(
        &mut self,
        observed: remuda_protocol::ObservedEffort,
        source: remuda_protocol::EffortSource,
        raw: Option<String>,
    ) -> DriverResult<Vec<Observation>> {
        let requested = self
            .effort_bridge
            .as_ref()
            .and_then(|bridge| bridge.requested())
            .map(|request| remuda_protocol::EffortSelection {
                name: request.name,
                ultracode: request.ultracode,
            });
        let payload = ObservationPayload::Effort(Box::new(EffortPayload {
            requested,
            effective: EffortEffective {
                name: observed.name,
                ultracode: observed.ultracode,
                source,
                observed_at: now()?,
            },
            raw,
        }));
        Ok(vec![self.mapper.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            payload,
        )?])
    }

    /// §9.1: read `effort` / `perTurnEffort` off an assistant record and emit
    /// an `effort` observation on edges. Assistant records corroborate the
    /// level (and launch-time effort) but never resolve a live switch — the
    /// command verdict does. Conversation mapping is unaffected.
    fn map_effort_assistant(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let effort = value.get("effort").and_then(Value::as_str);
        let per_turn = value.get("perTurnEffort").and_then(Value::as_str);
        let Some((observed, source)) = self.effort.observe(effort, per_turn) else {
            return Ok(Vec::new());
        };
        let raw = effort
            .filter(|value| !value.is_empty())
            .or(per_turn.filter(|value| !value.is_empty()))
            .map(str::to_owned);
        self.effort_observation(observed, source, raw)
    }

    // ───────── §9.1 model read-back (same verdict pattern as effort) ─────────

    /// Settle the armed model bridge and emit an edge observation for a
    /// verdict- or assistant-observed model id.
    fn model_observation(
        &mut self,
        observed: remuda_protocol::ObservedModel,
        source: EffortSource,
        raw: Option<String>,
    ) -> DriverResult<Vec<Observation>> {
        let (requested, selection_path) = self
            .model_bridge
            .as_ref()
            .map(|bridge| {
                let path = if source == EffortSource::Remuda {
                    bridge.requested_path()
                } else {
                    None
                };
                bridge.note_effective(observed.id.clone(), source, path);
                (bridge.requested().map(|request| request.id), path)
            })
            .unwrap_or((None, None));
        let payload = ObservationPayload::Model(Box::new(ModelPayload {
            requested,
            effective: EffectiveModel {
                id: observed.id,
                source,
                observed_at: now()?,
            },
            raw,
            // The catalog rides the launch snapshot; transcript edges never
            // re-send it.
            catalog: None,
            selection_path,
        }));
        Ok(vec![self.mapper.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            payload,
        )?])
    }

    /// Read `message.model` off an assistant record; emit an edge when the
    /// resolved id changes. Assistant records corroborate the verdict but
    /// never resolve a live switch on their own.
    fn map_model_assistant(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let model = value.pointer("/message/model").and_then(Value::as_str);
        let Some((observed, source)) = self.model.observe(model) else {
            return Ok(Vec::new());
        };
        // A post-switch assistant record can only arrive after the verdict; if
        // the verdict was somehow missed, settle the bridge here (resolved id
        // is what the next turn used).
        if let Some(generation) = self.model_generation.take()
            && let Some(bridge) = &self.model_bridge
        {
            bridge.resolve(generation, observed.clone());
        }
        self.model_observation(observed, source, model.map(str::to_owned))
    }

    /// A `/model` accepted on 2.1.272 writes TWO `user` records — slash markup
    /// and its `<local-command-stdout>` verdict. Same arm/settle split as
    /// `/effort`.
    fn note_model_user(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let Some(message) = value.get("message") else {
            return Ok(Vec::new());
        };
        let text = records::record_text(message);
        if text.contains("<command-name>/model</command-name>") {
            let Some(args) = remuda_protocol::slash_model_args(&text) else {
                return Ok(Vec::new());
            };
            let from_remuda = self
                .model_bridge
                .as_ref()
                .and_then(|bridge| bridge.pending())
                .is_some_and(|request| request.id == args);
            if from_remuda
                && let Some(bridge) = &self.model_bridge
                && let Some((generation, _request)) = bridge.pending_with_gen()
            {
                self.model_generation = Some(generation);
                self.model.arm_awaiting(&args);
            }
            self.model.note_slash(&args, from_remuda);
            return Ok(Vec::new());
        }
        if text.contains("<local-command-stdout>") {
            return self.settle_model_stdout(&extract_local_stdout(&text));
        }
        Ok(Vec::new())
    }

    /// Rejected switches (`Model '<id>' not found`) and a dismissed picker
    /// (`Kept model as <id>`) are `system` records with a top-level `content`
    /// string (`subtype: "local_command"`), not `user` records.
    fn note_model_system(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        if value.get("subtype").and_then(Value::as_str) != Some("local_command") {
            return Ok(Vec::new());
        }
        let Some(text) = value.get("content").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        if text.contains("<command-name>/model</command-name>") {
            let Some(args) = remuda_protocol::slash_model_args(text) else {
                return Ok(Vec::new());
            };
            let from_remuda = self
                .model_bridge
                .as_ref()
                .and_then(|bridge| bridge.pending())
                .is_some_and(|request| request.id == args);
            if from_remuda
                && let Some(bridge) = &self.model_bridge
                && let Some((generation, _request)) = bridge.pending_with_gen()
            {
                self.model_generation = Some(generation);
                self.model.arm_awaiting(&args);
            }
            self.model.note_slash(&args, from_remuda);
            return Ok(Vec::new());
        }
        if text.contains("<local-command-stdout>") {
            return self.settle_model_stdout(&extract_local_stdout(text));
        }
        Ok(Vec::new())
    }

    /// Shared verdict handling for `user` and `system` stdout records.
    fn settle_model_stdout(&mut self, stdout: &str) -> DriverResult<Vec<Observation>> {
        let verdict = remuda_protocol::parse_model_stdout(stdout);
        let from_remuda = self.model_generation.is_some()
            || self
                .model_bridge
                .as_ref()
                .and_then(|bridge| bridge.pending())
                .is_some();
        match verdict {
            remuda_protocol::ModelStdout::Accepted(observed) => {
                let edge = self.model.note_stdout(stdout, from_remuda);
                if let Some(generation) = self.model_generation.take()
                    && let Some(bridge) = &self.model_bridge
                {
                    bridge.resolve(generation, observed.clone());
                }
                if let Some((observed, source)) = edge {
                    return self.model_observation(observed, source, Some(stdout.to_owned()));
                }
            }
            remuda_protocol::ModelStdout::Kept | remuda_protocol::ModelStdout::NotFound => {
                self.model.note_stdout(stdout, from_remuda);
                if let Some(generation) = self.model_generation.take()
                    && let Some(bridge) = &self.model_bridge
                {
                    let reason = if verdict == remuda_protocol::ModelStdout::Kept {
                        "dialog-kept"
                    } else {
                        "not-found"
                    };
                    bridge.reject(generation, reason);
                }
            }
            remuda_protocol::ModelStdout::Other => {}
        }
        Ok(Vec::new())
    }

    /// Buffer an assistant record, flushing the previous run when superseded.
    fn map_assistant_record(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
        let message = value.get("message").cloned().unwrap_or(Value::Null);
        let Some(key) = records::GroupKey::of(&value, &message) else {
            // No `message.id` to group on — replay it on its own rather than
            // dropping a real assistant turn.
            let mut out = self.flush_group()?;
            out.extend(self.emit_conversation(value)?);
            return Ok(out);
        };
        if self.group.continues(&key) {
            self.group.push(&value, &message);
            return Ok(Vec::new());
        }
        let out = self.flush_group()?;
        self.group.start(key, value.clone());
        self.group.push(&value, &message);
        Ok(out)
    }

    /// Map a user record, dropping only exact re-deliveries of one prompt.
    fn map_user_record(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
        let message = value.get("message").cloned().unwrap_or(Value::Null);
        if message.is_null() {
            return Ok(Vec::new());
        }
        // A tail that re-reads the file must not re-emit what it already sent.
        if let Some(uuid) = value.get("uuid").and_then(Value::as_str)
            && !self.seen_uuids.insert(uuid.to_owned())
        {
            return Ok(Vec::new());
        }
        // The same human prompt lands twice when it is enqueued and then
        // delivered. Both records share a `promptId`, so the pair is only a
        // duplicate when the text matches too — two different prompts queued
        // under one id are two real messages.
        let origin = records::classify_user_record(&value, &message);
        if origin == remuda_protocol::MessageOrigin::Human
            && let Some(prompt_id) = value.get("promptId").and_then(Value::as_str)
        {
            let text = records::record_text(&message);
            if !text.trim().is_empty()
                && !self
                    .seen_prompts
                    .insert((prompt_id.to_owned(), text.clone()))
            {
                return Ok(Vec::new());
            }
        }
        self.emit_conversation(value)
    }

    /// `queue-operation` → a queue lifecycle carrying enqueue / dequeue / remove.
    ///
    /// §7: Remuda's own `pty_queue` stays the authority for the queue; this is
    /// the reconciliation evidence beside it, which is why it is journaled as a
    /// lifecycle rather than turned into a message.
    fn map_queue_operation(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let operation = value
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let mut related = std::collections::BTreeMap::new();
        related.insert("operation".into(), operation.to_owned());
        if let Some(reason) = value.get("reason").and_then(Value::as_str) {
            related.insert("reason".into(), reason.to_owned());
        }
        if let Some(content) = value.get("content").and_then(Value::as_str) {
            related.insert("content".into(), content.to_owned());
        }
        // The journal status the composer reads: an enqueue leaves the text
        // queued, everything else releases it.
        let status = match operation {
            "enqueue" => "queued",
            "dequeue" | "remove" => "dequeued",
            other => other,
        };
        let session_id = self.mapper.session_id.clone();
        Ok(vec![self.mapper.lifecycle_related(
            LifecycleTopic::Turn,
            "queue-operation",
            Knowledge::Known { value: session_id },
            status,
            related,
            false,
        )?])
    }

    /// `permission-mode` / `mode` → the effective-mode edge observation plus
    /// the existing permission lifecycle recording the drift.
    fn map_permission_mode(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let raw = value
            .get("permissionMode")
            .or_else(|| value.get("mode"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let session_id = self.mapper.session_id.clone();
        let lifecycle = self.mapper.lifecycle_related(
            LifecycleTopic::Permission,
            "permission-mode",
            Knowledge::Known { value: session_id },
            raw,
            std::collections::BTreeMap::new(),
            false,
        )?;

        // `mode: "normal"` records the TUI render mode, not a permission mode;
        // only permission vocabulary emits an effective edge.
        let Some(mode) = crate::permission::from_native(raw) else {
            return Ok(vec![lifecycle]);
        };
        let pending = self
            .permission_bridge
            .as_ref()
            .and_then(|bridge| bridge.pending_with_gen());
        let remuda_pending = pending.map(|(_, target)| target);
        let Some((observed, source)) = self.permission.note(raw, remuda_pending) else {
            // Even a deduped record settles a live switch the status line has
            // not been read for (e.g. right after launch).
            if let Some((generation, target)) = pending
                && target == mode
            {
                self.permission_bridge
                    .as_ref()
                    .expect("pending implies a bridge")
                    .resolve(generation, mode);
            }
            return Ok(vec![lifecycle]);
        };
        if let Some((generation, target)) = pending
            && target == observed
        {
            self.permission_bridge
                .as_ref()
                .expect("pending implies a bridge")
                .resolve(generation, observed);
        }
        let mut out = self.permission_observation(observed, source, Some(raw.to_owned()))?;
        out.push(lifecycle);
        Ok(out)
    }

    /// Build the effective permission-mode observation for one edge.
    fn permission_observation(
        &mut self,
        mode: ClaudePermissionMode,
        source: remuda_protocol::PermissionSource,
        raw: Option<String>,
    ) -> DriverResult<Vec<Observation>> {
        let requested = self
            .permission_bridge
            .as_ref()
            .and_then(|bridge| bridge.armed())
            .map(|(_, target)| crate::permission::wire_word(target).to_owned());
        let payload =
            ObservationPayload::Permission(Box::new(remuda_protocol::PermissionPayload {
                requested,
                effective: remuda_protocol::PermissionEffective {
                    mode: crate::permission::wire_word(mode).to_owned(),
                    source,
                    observed_at: now()?,
                },
                raw,
            }));
        Ok(vec![self.mapper.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            payload,
        )?])
    }

    /// `attachment` → only `queued_command` is journaled.
    ///
    /// §7 [V]: `attachment.queued_command` is the delivery evidence for a
    /// queued message and is **not** a `user` record, so a mapper that only
    /// tails user/assistant misses it entirely. It keeps the *enqueue*
    /// timestamp, so it is ordered by file position, never by its own clock.
    /// The other attachment subtypes (`hook_success`, `skill_listing`,
    /// `agent_listing_delta`, …) are Claude's own bookkeeping.
    fn map_attachment(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let attachment = value.get("attachment").unwrap_or(&Value::Null);
        if attachment.get("type").and_then(Value::as_str) != Some("queued_command") {
            return Ok(Vec::new());
        }
        let mut related = std::collections::BTreeMap::new();
        related.insert("operation".into(), "delivered".into());
        for (key, field) in [("content", "command"), ("content", "content")] {
            if let Some(text) = attachment.get(field).and_then(Value::as_str) {
                related.insert(key.into(), text.to_owned());
                break;
            }
        }
        let session_id = self.mapper.session_id.clone();
        Ok(vec![self.mapper.lifecycle_related(
            LifecycleTopic::Turn,
            "queued-command",
            Knowledge::Known { value: session_id },
            "dequeued",
            related,
            false,
        )?])
    }
}

/// Mapping / gating helpers used by `tests/claude_print_review.rs`.
#[doc(hidden)]
pub mod review {
    use super::*;

    /// A stdout mapper that keeps its state across frames.
    ///
    /// [`map_stdout_json`] builds a fresh mapper per call, so native ids never
    /// correlate — a `tool_result` in a later frame cannot find the
    /// `tool_use` that preceded it. That is fine for checking one frame's
    /// shape, but a multi-frame scenario (the D-028 §12 parity gate) needs the
    /// same mapper throughout, exactly as the live reader has.
    pub struct StdoutMapper {
        mapper: Mapper,
    }

    impl Default for StdoutMapper {
        fn default() -> Self {
            Self::new()
        }
    }

    impl StdoutMapper {
        /// A mapper stamping `claude-print` / `stdout` observations.
        #[must_use]
        pub fn new() -> Self {
            Self {
                mapper: Mapper {
                    stream: stream::StreamState::default(),
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
                    driver_kind: DriverKind::ClaudePrint,
                    channel: SourceChannel::Stdout,
                },
            }
        }

        /// Map one stdout frame, carrying identity forward.
        pub fn map(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
            map_outbound(&mut self.mapper, &Outbound::from_value(value))
        }
    }

    /// Map one stdout JSON object the same way the live reader does.
    pub fn map_stdout_json(value: Value) -> DriverResult<Vec<Observation>> {
        let frame = Outbound::from_value(value);
        let mut mapper = Mapper {
            stream: stream::StreamState::default(),
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
            driver_kind: DriverKind::ClaudePrint,
            channel: SourceChannel::Stdout,
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

#[cfg(test)]
mod attachment_tests {
    use super::*;
    use remuda_protocol::{Knowledge, MediaBlock, ResourceBlock, TextBlock};

    /// 1x1 red PNG. Small, real, and enough to prove the bytes round-trip.
    const RED_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn image_blocks(path: &std::path::Path, text: &str) -> Vec<ContentBlock> {
        let id = remuda_protocol::Id::new("obj").expect("id");
        vec![
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: id.clone(),
                media_type: "image/png".into(),
                name: Some("shot.png".into()),
                anchor: None,
                size: None,
            })),
            ContentBlock::Resource(Box::new(ResourceBlock {
                uri: format!("file://{}", path.display()),
                media_type: Knowledge::Known {
                    value: "image/png".into(),
                },
                object_id: Some(id),
            })),
            ContentBlock::Text(Box::new(TextBlock { text: text.into() })),
        ]
    }

    /// A text-only prompt keeps the plain-string shape the CLI has always got.
    #[test]
    fn a_prompt_without_attachments_is_still_sent_as_text() {
        let blocks = vec![ContentBlock::Text(Box::new(TextBlock {
            text: "hello".into(),
        }))];
        match prompt_content(&blocks).expect("content") {
            UserContent::Text(text) => assert_eq!(text, "hello"),
            UserContent::Blocks(blocks) => panic!("expected plain text, got {blocks:?}"),
        }
    }

    /// D-027: an attachment becomes a base64 image block ahead of the text.
    /// Base64 specifically — the design verified that a `file` source or a
    /// bare path is silently downgraded to text by the CLI.
    #[test]
    fn an_attachment_becomes_a_base64_image_block_before_the_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shot.png");
        std::fs::write(&path, RED_PNG).expect("write png");

        let content =
            prompt_content(&image_blocks(&path, "what colour is the image?")).expect("content");
        let UserContent::Blocks(blocks) = content else {
            panic!("expected content blocks");
        };
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], serde_json::json!("image"));
        assert_eq!(blocks[0]["source"]["type"], serde_json::json!("base64"));
        assert_eq!(
            blocks[0]["source"]["media_type"],
            serde_json::json!("image/png")
        );
        let encoded = blocks[0]["source"]["data"].as_str().expect("data");
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("decode"),
            RED_PNG,
            "the CLI must receive the exact bytes that were staged"
        );
        assert_eq!(blocks[1]["type"], serde_json::json!("text"));
        assert_eq!(
            blocks[1]["text"],
            serde_json::json!("what colour is the image?")
        );
    }

    /// Past the inline budget the driver mentions paths instead, so the image
    /// is still reachable through the Read tool rather than simply dropped.
    #[test]
    fn oversized_attachments_fall_back_to_a_path_mention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.png");
        let mut big = RED_PNG.to_vec();
        big.resize(MAX_INLINE_IMAGE_BYTES + 1, 0);
        std::fs::write(&path, &big).expect("write png");

        match prompt_content(&image_blocks(&path, "describe it")).expect("content") {
            UserContent::Text(text) => {
                assert!(text.contains("describe it"), "{text}");
                assert!(text.contains(&path.display().to_string()), "{text}");
            }
            UserContent::Blocks(blocks) => panic!("expected a path mention, got {blocks:?}"),
        }
    }

    /// A staged file that vanished is an error, not a silently text-only turn.
    #[test]
    fn a_missing_attachment_file_fails_the_send() {
        let blocks = image_blocks(std::path::Path::new("/nonexistent/shot.png"), "hi");
        assert!(prompt_content(&blocks).is_err());
    }
}
