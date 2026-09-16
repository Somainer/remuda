//! Local Node composition and per-Instance task supervision.

use crate::{
    CommandAction, CreateInstanceRequest, CreateInstanceResponse, Driver, DriverEmission,
    DriverLaunch, DriverRegistry, DriverRequest, InstanceCommandRequest, InteractionRuntime,
    LocalStore, MemoryStore, NodeError, TtyRegistry,
    store::{timestamp_now, unknown},
};
use futures::FutureExt;
use remuda_protocol::{
    Acceptance, AcceptanceScope, Activity, ActorRef, ActorType, AgentKind, ClaudeRef, Command,
    CommandAuthority, CommandId, CommandOperation, CommandOrigin, CommandResult, CommandState,
    CommandTarget, Completeness, Connectivity, Digest as WireDigest, DispatchState,
    EntityLifecycle, EntityMeta, ExpectedState, Host, HostId, HostState, HostTransport,
    HostTransportMode, Id, Instance, InstanceId, InstanceLifecycle, JournalEvent, Knowledge,
    LifecycleEntity, LifecyclePayload, MessagePhase, MessageRole, NativeRef, NodeReceipt,
    ObservationPayload, Ownership, Page, PathStyle, Platform, ProcessRef, ResolutionState,
    Settlement, SettlementOutcome, U64, Workspace, WorkspaceId, WorkspaceState, WritePolicy,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio::sync::{RwLock, mpsc};

mod pty_queue;

struct QueuedCommand {
    command_id: CommandId,
    request: DriverRequest,
    close_after: bool,
    /// D-027 attachment refs from the Hub. Bytes are pulled in the instance
    /// worker, never on the RPC path: a slow object fetch must not delay the
    /// durable accept (or serialize the runtime link behind it).
    attachment_refs: Vec<remuda_protocol::hubnode::AttachmentRef>,
}

/// Hub object source plus the Node data dir, used inside an instance worker to
/// pull attachment bytes after the send has been durably accepted (D-027).
#[derive(Clone)]
pub(crate) struct AttachmentLoader {
    source: Option<Arc<dyn crate::attachments::ObjectSource>>,
    data_dir: Option<std::path::PathBuf>,
}

impl AttachmentLoader {
    fn new(
        source: Option<Arc<dyn crate::attachments::ObjectSource>>,
        data_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self { source, data_dir }
    }

    /// Pull a send's attachment bytes in the worker. Runs off the RPC path, so
    /// a slow Hub object store delays only the affected command.
    async fn resolve(
        &self,
        instance_id: &InstanceId,
        refs: Vec<remuda_protocol::hubnode::AttachmentRef>,
    ) -> Result<Vec<crate::attachments::MaterializedAttachment>, NodeError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let source = self.source.as_ref().ok_or_else(|| {
            NodeError::InvalidRequest(
                "this Node has no Hub attachment source; send without attachments".into(),
            )
        })?;
        let data_dir = self.data_dir.as_ref().ok_or_else(|| {
            NodeError::InvalidRequest(
                "attachments need a Node data directory; none is configured".into(),
            )
        })?;
        crate::attachments::materialize(source, data_dir, instance_id, &refs).await
    }
}

pub(crate) struct DevNodeInner {
    pub(crate) store: Arc<dyn LocalStore>,
    drivers: DriverRegistry,
    interactions: Arc<InteractionRuntime>,
    senders: RwLock<BTreeMap<InstanceId, mpsc::Sender<QueuedCommand>>>,
    pub(crate) workers: tokio::sync::Mutex<BTreeMap<InstanceId, tokio::task::JoinHandle<()>>>,
    /// Observation pumps, one per started instance.
    ///
    /// Tracked rather than detached because a pump owns an `Arc<dyn LocalStore>`
    /// and writes to it. A pump that outlives its Node keeps that store — and
    /// therefore the journal's SQLite handle and its in-memory `durable_seq`
    /// mirror — alive after a second Node has reopened the same data dir, and
    /// the two then allocate the same sequence number. That surfaces as
    /// `UNIQUE constraint failed: events.instance_id, events.seq`.
    pub(crate) pumps: Arc<tokio::sync::Mutex<BTreeMap<InstanceId, tokio::task::JoinHandle<()>>>>,
    pub(crate) instance_drivers: RwLock<BTreeMap<InstanceId, Arc<dyn Driver>>>,
    pub(crate) stopping: std::sync::atomic::AtomicBool,
    pub(crate) mutations: RwLock<()>,
    pub(crate) herdr_config: Option<crate::NativeDriverConfig>,
    /// Where staged attachment bytes are pulled from (D-027). `None` on a Node
    /// with no Hub link, where a send carrying attachments is refused rather
    /// than silently downgraded to text.
    pub(crate) objects: std::sync::RwLock<Option<Arc<dyn crate::attachments::ObjectSource>>>,
    /// Root for materialized attachments. Set by `compose` from the Node data
    /// dir; `None` on an in-memory Node, where attachments are refused.
    pub(crate) attachment_root: std::sync::RwLock<Option<std::path::PathBuf>>,
    queue_capacity: usize,
    host: Host,
    workspace: Workspace,
    pub(crate) workspace_registry: std::sync::RwLock<crate::workspace::WorkspaceRegistry>,
    projection_epoch: Id,
    tty: TtyRegistry,
    /// Batch 6 lane gate runner state (lane locks, live jobs, event uplink).
    pub(crate) gate: crate::gate::GateRegistry,
    diagnostics: std::sync::RwLock<crate::DoctorContext>,
}

/// In-process Node used by the development REST/JSON-RPC/WS surface.
#[derive(Clone)]
pub struct DevNode {
    pub(crate) inner: Arc<DevNodeInner>,
}

impl DevNode {
    /// Build a Node with a bounded in-memory store and the default FakeDriver.
    pub fn new(config: &crate::DevServerConfig) -> Result<Self, NodeError> {
        Self::with_host_id(config, HostId::new())
    }

    /// Build a Node whose Host identity matches a persisted enrollment.
    pub fn with_host_id(
        config: &crate::DevServerConfig,
        host_id: HostId,
    ) -> Result<Self, NodeError> {
        let store = Arc::new(MemoryStore::new(config.follow_buffer_capacity));
        let drivers = DriverRegistry::with_fake()?;
        Self::with_parts_on_host(config, store, drivers, host_id)
    }

    /// Build a Node around externally supplied store and driver trait objects.
    pub fn with_parts(
        config: &crate::DevServerConfig,
        store: Arc<dyn LocalStore>,
        drivers: DriverRegistry,
    ) -> Result<Self, NodeError> {
        Self::with_parts_on_host(config, store, drivers, HostId::new())
    }

    /// Build a Node around supplied parts and a persisted Host identity.
    pub fn with_parts_on_host(
        config: &crate::DevServerConfig,
        store: Arc<dyn LocalStore>,
        drivers: DriverRegistry,
        host_id: HostId,
    ) -> Result<Self, NodeError> {
        if config.instance_queue_capacity == 0 {
            return Err(NodeError::InvalidConfig(
                "instance queue capacity must be positive".to_owned(),
            ));
        }
        let host = fixture_host(host_id.clone())?;
        let workspace_registry = crate::workspace::WorkspaceRegistry::open(config, host_id)?;
        let workspace = workspace_registry
            .workspaces()
            .into_iter()
            .next()
            .ok_or_else(|| {
                NodeError::InvalidConfig("initial workspace registry is empty".into())
            })?;
        let interactions = InteractionRuntime::spawn(Arc::clone(&store))?;
        Ok(Self {
            inner: Arc::new(DevNodeInner {
                store,
                drivers,
                interactions,
                senders: RwLock::new(BTreeMap::new()),
                workers: Default::default(),
                pumps: Default::default(),
                instance_drivers: Default::default(),
                stopping: Default::default(),
                mutations: Default::default(),
                herdr_config: None,
                objects: std::sync::RwLock::new(None),
                attachment_root: std::sync::RwLock::new(None),
                queue_capacity: config.instance_queue_capacity,
                host,
                workspace,
                workspace_registry: std::sync::RwLock::new(workspace_registry),
                projection_epoch: Id::new("epoch")?,
                tty: TtyRegistry::new(),
                gate: crate::gate::GateRegistry::new(),
                diagnostics: std::sync::RwLock::new(crate::DoctorContext::default()),
            }),
        })
    }

    /// Return the local development Host registry record.
    pub fn host(&self) -> Host {
        self.inner.host.clone()
    }

    /// Return the local development Workspace registry record.
    pub fn workspace(&self) -> Workspace {
        self.inner.workspace.clone()
    }

    /// Per-instance TTY bridges (herdr control / shell-pty).
    #[must_use]
    pub fn tty(&self) -> &TtyRegistry {
        &self.inner.tty
    }

    /// Lane gate runner state (batch 6 co-lanes).
    #[must_use]
    pub(crate) fn gate_registry(&self) -> &crate::gate::GateRegistry {
        &self.inner.gate
    }

    /// Set diagnostics inputs from trusted process composition, never RPC params.
    pub fn configure_doctor(&self, context: crate::DoctorContext) -> Result<(), NodeError> {
        *self
            .inner
            .diagnostics
            .write()
            .map_err(|_| NodeError::InvalidConfig("diagnostics lock poisoned".into()))? = context;
        Ok(())
    }

    /// Fresh local preflight for an authenticated Hub request.
    pub async fn doctor(&self) -> Result<Value, NodeError> {
        let context = self
            .inner
            .diagnostics
            .read()
            .map_err(|_| NodeError::InvalidConfig("diagnostics lock poisoned".into()))?
            .clone();
        let workspaces = self
            .workspaces()?
            .into_iter()
            .map(|workspace| std::path::PathBuf::from(workspace.root_path))
            .collect::<Vec<_>>();
        let report = tokio::task::spawn_blocking(move || {
            crate::diagnostics::doctor_with_workspaces(
                &context,
                &workspaces,
                crate::ProbeEnv::from_process(),
            )
        })
        .await
        .map_err(|error| NodeError::InvalidRequest(error.to_string()))?;
        serde_json::to_value(report).map_err(NodeError::from)
    }

    /// Persisted launch recipe for an Instance, if the driver wrote one.
    pub fn launch_recipe(
        &self,
        instance_id: &InstanceId,
    ) -> Result<Option<remuda_driver::LaunchRecipe>, NodeError> {
        self.inner.store.launch_recipe(instance_id)
    }

    /// List current Instances.
    pub fn list_instances(&self) -> Result<Page<Instance>, NodeError> {
        Ok(Page {
            items: self.inner.store.list_instances()?,
            next_cursor: None,
        })
    }

    /// Read one Instance.
    pub fn get_instance(&self, instance_id: &InstanceId) -> Result<Instance, NodeError> {
        self.inner.store.get_instance(instance_id)
    }

    /// Remove every Node-owned trace of a stopped Instance.
    ///
    /// Deletes the Node's own rows (instance, commands, launch recipe) and the
    /// per-instance data directory holding launch artifacts and PTY logs. It
    /// refuses while the Instance is still live, so a purge can never race a
    /// running driver.
    ///
    /// The agent's own native transcripts live under the user's home
    /// (`~/.claude`, `~/.codex`, …), outside the Node data directory, and are
    /// deliberately never touched: deleting a Remuda session must not delete
    /// the user's own agent history.
    pub async fn purge_instance(&self, instance_id: &InstanceId) -> Result<Value, NodeError> {
        // A delete usually arrives right behind `instance.close`, so give the
        // driver a moment to finish exiting rather than refusing a purge that
        // is only milliseconds early.
        for _ in 0..50 {
            match self.inner.store.get_instance(instance_id) {
                Ok(instance)
                    if matches!(
                        instance.lifecycle,
                        InstanceLifecycle::Exited | InstanceLifecycle::Failed
                    ) =>
                {
                    break;
                }
                Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                Err(_) => break,
            }
        }
        // Still live after the grace. The Hub only purges behind a delete it
        // has already decided to perform (`?force=1` stops the instance
        // first), so refusing here does not keep the session alive — it just
        // orphans the process: the Hub drops its row anyway and logs
        // "node rejected instance.purge", and the Node is left holding a
        // running agent nobody can reach. That is what left a claude process
        // parented to the Node after every forced delete on macOS, where the
        // shell-pty stop ladder (SIGINT 2s + SIGHUP 2s + SIGKILL + reap) takes
        // longer than this grace.
        //
        // So close it here and then purge. Closing a driver is idempotent and
        // is exactly what the queued `instance.close` would have done.
        if let Ok(instance) = self.inner.store.get_instance(instance_id)
            && !matches!(
                instance.lifecycle,
                InstanceLifecycle::Exited | InstanceLifecycle::Failed
            )
        {
            let driver = self
                .inner
                .instance_drivers
                .read()
                .await
                .get(instance_id)
                .cloned();
            match driver {
                Some(driver) => {
                    tracing::info!(
                        instance = %instance_id.as_id(),
                        lifecycle = ?instance.lifecycle,
                        "purge is closing a still-live instance rather than orphaning its process"
                    );
                    if let Err(error) = driver.execute(DriverRequest::Close).await {
                        // The process may still be there, so the directory must
                        // not be removed under it.
                        return Err(NodeError::Driver(format!(
                            "purge could not stop the instance: {error}"
                        )));
                    }
                    finish_instance_operation(
                        self.inner.store.as_ref(),
                        instance_id,
                        &DriverRequest::Close,
                    )?;
                }
                None => {
                    // No live driver to close: nothing of this instance is
                    // running here, so the rows and directory are safe to drop.
                    tracing::warn!(
                        instance = %instance_id.as_id(),
                        lifecycle = ?instance.lifecycle,
                        "purging an instance with no live driver; its lifecycle row was stale"
                    );
                }
            }
        }
        let removed = self.inner.store.remove_instance(instance_id)?;
        let mut directory_removed = false;
        if let Some(config) = &self.inner.herdr_config {
            let dir = config
                .data_dir
                .join("instances")
                .join(instance_id.as_id().as_str());
            // `instances/<id>` is Node-owned: launch recipes, overlays, pty
            // logs. Never a path the user chose.
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => directory_removed = true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(NodeError::Driver(format!(
                        "purge could not remove the instance directory: {error}"
                    )));
                }
            }
        }
        Ok(serde_json::json!({
            "purged": removed,
            "directoryRemoved": directory_removed,
        }))
    }

    /// Current screen of a PTY-carried Instance, as text.
    ///
    /// The `shell-pty` carrier's missing debugging tool: when an agent parks on
    /// a dialog before its first hook fires the journal is empty, and the
    /// screen is the only evidence of why. Read-only — no attach, no offset
    /// movement, no keystroke — so it cannot disturb the session it inspects.
    ///
    /// A driver with no screen (`claude-print`, the fakes) answers
    /// `supported: false` rather than an empty grid: a blank terminal and
    /// "this carrier has no terminal" are different facts.
    pub async fn screen_read(&self, instance_id: &InstanceId) -> Result<Value, NodeError> {
        // Resolve the instance first so an unknown id is a 404-shaped error
        // rather than "no live driver".
        let instance = self.inner.store.get_instance(instance_id)?;
        let driver = self
            .inner
            .instance_drivers
            .read()
            .await
            .get(instance_id)
            .cloned();
        let Some(driver) = driver else {
            return Ok(serde_json::json!({
                "instanceId": instance_id,
                "supported": false,
                "reason": "no live driver for this instance",
                "lifecycle": instance.lifecycle,
            }));
        };
        let screen = driver
            .screen_read()
            .await
            .map_err(|error| NodeError::Driver(error.to_string()))?;
        match screen {
            Some(screen) => Ok(serde_json::json!({
                "instanceId": instance_id,
                "supported": true,
                "lifecycle": instance.lifecycle,
                "driver": driver.kind(),
                "cols": screen.cols,
                "rows": screen.rows,
                "cursor": {"row": screen.cursor.0, "col": screen.cursor.1},
                "altScreen": screen.alt_screen,
                "source": if screen.emulated { "emulator" } else { "raw-ring" },
                "lines": screen.lines,
            })),
            None => Ok(serde_json::json!({
                "instanceId": instance_id,
                "supported": false,
                "reason": "this driver carries no readable screen",
                "lifecycle": instance.lifecycle,
                "driver": driver.kind(),
            })),
        }
    }

    /// Catalog of git worktrees under the first registered workspace root.
    pub fn list_worktrees(&self) -> Result<Value, NodeError> {
        self.worktree_rpc("worktree.list", &serde_json::json!({}))
    }

    /// Create or reuse a git worktree under the selected registered workspace.
    pub fn create_worktree(&self, params: &Value) -> Result<Value, NodeError> {
        self.worktree_rpc("worktree.create", params)
    }

    pub(crate) fn worktree_rpc(&self, method: &str, params: &Value) -> Result<Value, NodeError> {
        let selected = params
            .get("workspaceId")
            .and_then(Value::as_str)
            .map(str::parse)
            .transpose()?;
        let (workspace, _) = self.resolve_workspace_cwd(selected.as_ref(), None)?;
        crate::worktree::handle_rpc(Path::new(&workspace.root_path), method, params)
            .ok_or_else(|| NodeError::InvalidRequest(format!("unknown method {method}")))?
    }

    /// Durably accept an Instance create, then materialize it in its worker.
    ///
    /// D-028 P2 follow-up: the reply must prove only the durable accept —
    /// instance row, accepted command ledger entry, and its journal event.
    /// Everything that can be slow (driver build, binary pin, overlay/shim
    /// generation, PTY spawn) happens in the instance worker afterwards, with
    /// progress journaled `preparing → starting → ready` (protocol §2.3).
    /// A loaded host must never time this out at the Hub's 5 s accept deadline.
    pub async fn create_instance(
        &self,
        request: CreateInstanceRequest,
    ) -> Result<CreateInstanceResponse, NodeError> {
        let span = tracing::info_span!(
            "instance.create",
            kind = ?request.kind,
            driver = ?request.driver
        );
        use tracing::Instrument;
        self.create_instance_inner(request).instrument(span).await
    }

    async fn create_instance_inner(
        &self,
        request: CreateInstanceRequest,
    ) -> Result<CreateInstanceResponse, NodeError> {
        let _mutation = self.inner.mutations.read().await;
        if self
            .inner
            .stopping
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(NodeError::InvalidRequest("Node is shutting down".into()));
        }
        let payload_digest = digest_json(&request)?;
        validate_kind_driver(request.kind, request.driver)?;
        validate_text(&request.prompt, "prompt")?;

        let host_id = request
            .host_id
            .clone()
            .unwrap_or_else(|| self.inner.host.meta.id.clone());
        let (workspace, workspace_root) =
            self.resolve_workspace_cwd(request.workspace_id.as_ref(), request.cwd.as_deref())?;
        let workspace_id = workspace.meta.id.clone();
        if host_id != self.inner.host.meta.id {
            return Err(NodeError::InvalidRequest(
                "create hostId is not the local development Host".to_owned(),
            ));
        }

        let instance_id = request.instance_id.clone().unwrap_or_default();
        let mut instance = fixture_instance_with_session(
            instance_id.clone(),
            host_id.clone(),
            workspace_id,
            request.driver,
            request.resume_session_id.as_deref(),
        )?;
        // D-026: the exited Instance keeps its history; the resumed one records
        // where its conversation came from so both ends of the link are durable.
        if let Some(parent) = request.resumed_from.clone() {
            instance.parent = Some(remuda_protocol::InstanceParent {
                instance_id: parent,
                run_id: remuda_protocol::RunId::new(),
                command_id: request.command_id.clone().unwrap_or_default(),
            });
        }
        // Protocol §2.3: accepted-but-not-yet-materialized. The worker moves
        // this through `starting` and `ready`; the Hub mirrors those states
        // from the journal even when this RPC's reply arrives late.
        instance.lifecycle = InstanceLifecycle::Preparing;
        let launch = DriverLaunch {
            instance: instance.clone(),
            request: request.clone(),
            workspace_root,
            registered_workspace_root: workspace.root_path.into(),
        };
        self.inner.store.insert_instance(instance)?;

        let command_id = request.command_id.clone().unwrap_or_default();
        let mut command = new_command(
            command_id.clone(),
            CommandOperation::InstanceCreate,
            &instance_id,
            &host_id,
            &self.inner.projection_epoch,
            None,
            payload_digest,
        )?;
        set_command_origin(&mut command, request.origin);
        let inserted = self
            .inner
            .store
            .insert_command(&instance_id, command.clone())?;
        if !inserted {
            return Ok(CreateInstanceResponse {
                command: self.inner.store.get_command(&command_id)?,
                instance: self.inner.store.get_instance(&instance_id)?,
            });
        }
        accept_command(&mut command)?;
        // Journal first, reply second: a Hub that times out the RPC converges
        // from these rows instead of leaving `requested / unknown` forever.
        self.inner.store.save_command(command.clone())?;
        append_command_lifecycle(
            self.inner.store.as_ref(),
            &instance_id,
            &command,
            "accepted",
        )?;
        append_instance_lifecycle(
            self.inner.store.as_ref(),
            &instance_id,
            Some("requested"),
            "preparing",
            "create-accepted",
        )?;
        self.spawn_instance_worker(instance_id.clone(), launch, command.clone(), request.prompt)
            .await;
        let instance = self.inner.store.get_instance(&instance_id)?;
        Ok(CreateInstanceResponse { command, instance })
    }

    /// Point this Node at the Hub's attachment store (D-027).
    ///
    /// Set once the outbound link knows its Hub URL and host token. Until it
    /// is set, a send that carries attachments is refused.
    pub fn set_object_source(&self, source: Arc<dyn crate::attachments::ObjectSource>) {
        if let Ok(mut slot) = self.inner.objects.write() {
            *slot = Some(source);
        }
    }

    /// Root under which this Node materializes attachments (D-027).
    ///
    /// Explicitly configured rather than inferred, so the fake-driver Node
    /// used by tests and `--stdio` can have one too.
    pub fn set_attachment_root(&self, root: std::path::PathBuf) {
        if let Ok(mut slot) = self.inner.attachment_root.write() {
            *slot = Some(root);
        }
    }

    /// Carrier supervisor for Instance workers, when this Node owns a Herdr
    /// session. A fake-driver Node has no carrier to recover.
    fn carrier_supervisor(&self) -> Option<crate::carrier_recovery::CarrierSupervisor> {
        self.inner
            .herdr_config
            .as_ref()
            .map(|_| crate::carrier_recovery::CarrierSupervisor::new(&self.inner))
    }

    /// Node data directory, when one is configured.
    fn data_dir(&self) -> Option<std::path::PathBuf> {
        if let Ok(slot) = self.inner.attachment_root.read()
            && let Some(root) = slot.clone()
        {
            return Some(root);
        }
        self.inner
            .herdr_config
            .as_ref()
            .map(|config| config.data_dir.clone())
    }

    /// Submit send/cancel/respond/close through an Instance's bounded queue.
    pub async fn submit_command(
        &self,
        instance_id: &InstanceId,
        request: InstanceCommandRequest,
    ) -> Result<CommandResult, NodeError> {
        let _mutation = self.inner.mutations.read().await;
        if self
            .inner
            .stopping
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(NodeError::InvalidRequest("Node is shutting down".into()));
        }
        let instance = self.inner.store.get_instance(instance_id)?;
        // D-027: attachment bytes are pulled by the instance worker *after* the
        // command is durably accepted, so a slow Hub object fetch cannot delay
        // this reply or serialize the runtime link.
        let attachment_refs = request.attachments.clone();
        let (operation, driver_request, close_after) = command_parts(&request, Vec::new())?;
        let command_id = request.command_id.clone().unwrap_or_default();
        let mut command = new_command(
            command_id.clone(),
            operation,
            instance_id,
            &instance.host_id,
            &self.inner.projection_epoch,
            request.run_id.clone(),
            digest_json(&request)?,
        )?;
        set_command_origin(&mut command, request.origin);
        let inserted = self
            .inner
            .store
            .insert_command(instance_id, command.clone())?;
        if !inserted {
            return Ok(CommandResult {
                command: self.inner.store.get_command(&command_id)?,
                related_command_ids: Vec::new(),
            });
        }

        let result = self
            .enqueue_existing(
                instance_id,
                command_id,
                driver_request,
                close_after,
                attachment_refs,
            )
            .await;
        match result {
            Ok(command) => Ok(CommandResult {
                command,
                related_command_ids: Vec::new(),
            }),
            Err(NodeError::QueueFull) => {
                let command = reject_before_dispatch(
                    self.inner.store.as_ref(),
                    instance_id,
                    command,
                    "instance-queue-full",
                )?;
                Ok(CommandResult {
                    command,
                    related_command_ids: Vec::new(),
                })
            }
            Err(NodeError::DriverUnavailable)
                if close_after
                    && self
                        .inner
                        .workers
                        .lock()
                        .await
                        .get(instance_id)
                        .is_none_or(|worker| worker.is_finished()) =>
            {
                self.close_ended_instance(instance_id, "explicit-close")
                    .await?;
                let mut command = command;
                accept_command(&mut command)?;
                settle_command(
                    &mut command,
                    SettlementOutcome::Completed,
                    None,
                    remuda_protocol::ExecutionState::PossiblyDispatched,
                )?;
                self.inner.store.save_command(command.clone())?;
                append_command_lifecycle(
                    self.inner.store.as_ref(),
                    instance_id,
                    &command,
                    "settled",
                )?;
                Ok(CommandResult {
                    command,
                    related_command_ids: Vec::new(),
                })
            }
            Err(NodeError::DriverUnavailable) => {
                let command = reject_before_dispatch(
                    self.inner.store.as_ref(),
                    instance_id,
                    command,
                    "instance-driver-unavailable",
                )?;
                Ok(CommandResult {
                    command,
                    related_command_ids: Vec::new(),
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Read one Command for timeout reconciliation.
    pub fn get_command(&self, command_id: &CommandId) -> Result<Command, NodeError> {
        self.inner.store.get_command(command_id)
    }

    /// Read one exclusive sequence page from a journal.
    pub fn read_journal(
        &self,
        journal_id: &Id,
        after_seq: Option<U64>,
        limit: usize,
    ) -> Result<remuda_protocol::EventsReadResult, NodeError> {
        self.inner.store.read_events(journal_id, after_seq, limit)
    }

    /// Resolve a journal to its Instance.
    pub fn instance_for_journal(&self, journal_id: &Id) -> Result<InstanceId, NodeError> {
        self.inner.store.instance_for_journal(journal_id)
    }

    /// Subscribe to one Instance before taking its snapshot.
    pub fn subscribe(
        &self,
        instance_id: &InstanceId,
    ) -> Result<tokio::sync::broadcast::Receiver<JournalEvent>, NodeError> {
        self.inner.store.subscribe(instance_id)
    }

    /// Build an Instance snapshot at the store's current durable sequence.
    pub fn snapshot(
        &self,
        instance_id: &InstanceId,
    ) -> Result<remuda_protocol::InstanceSnapshot, NodeError> {
        self.inner
            .store
            .snapshot(instance_id, self.inner.projection_epoch.clone())
    }

    /// Projection epoch shared by snapshots from this local Node process.
    pub fn projection_epoch(&self) -> Id {
        self.inner.projection_epoch.clone()
    }

    /// Hub→Node `interaction.list` / `interaction.answer`.
    pub async fn dispatch_interaction(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, NodeError> {
        self.inner.interactions.dispatch_rpc(method, params).await
    }

    async fn spawn_instance_worker(
        &self,
        instance_id: InstanceId,
        launch: DriverLaunch,
        create_command: Command,
        initial_prompt: String,
    ) {
        let (sender, receiver) = mpsc::channel(self.inner.queue_capacity);
        self.inner
            .senders
            .write()
            .await
            .insert(instance_id.clone(), sender);
        let store = self.inner.store.clone();
        let drivers = self.inner.drivers.clone();
        let interactions = Arc::clone(&self.inner.interactions);
        let tty = self.inner.tty.clone();
        let data_dir = self.data_dir();
        let objects = self.inner.objects.read().ok().and_then(|slot| slot.clone());
        let worker_instance = instance_id.clone();
        let carrier = self.carrier_supervisor();
        let pumps = Arc::clone(&self.inner.pumps);
        let node = Arc::downgrade(&self.inner);
        let worker = tokio::spawn(async move {
            // Building the driver is materialization, not acceptance: it runs
            // `pin_binary` (a ~207 MB SHA-256 the first time, plus a blocking
            // `--version` subprocess), writes the settings overlay and launch
            // shims, and prepares the native home. None of it is allowed to
            // stand between the Hub and the durable accept, so it happens
            // here, after the reply was authorized — and on a blocking thread,
            // so a 20-second `--version` cannot stall a runtime core.
            let build_span = tracing::info_span!("build_driver", driver = ?launch.request.driver);
            let build = tokio::task::spawn_blocking(move || {
                let _span = build_span.entered();
                drivers.build(launch.request.driver, launch.clone())
            });
            use tracing::Instrument;
            let driver = match build
                .instrument(tracing::info_span!("build_driver_wait"))
                .await
            {
                Ok(Ok(driver)) => driver,
                Ok(Err(error)) => {
                    tracing::error!(%error, "instance driver build failed");
                    record_task_exit(store.as_ref(), &worker_instance, &error.to_string());
                    if let Some(node) = node.upgrade() {
                        node.senders.write().await.remove(&worker_instance);
                    }
                    return;
                }
                Err(join_error) => {
                    let reason = format!("driver build task panicked: {join_error}");
                    tracing::error!(%reason, "instance driver build panicked");
                    record_task_exit(store.as_ref(), &worker_instance, &reason);
                    if let Some(node) = node.upgrade() {
                        node.senders.write().await.remove(&worker_instance);
                    }
                    return;
                }
            };
            driver.track_pty_resources(
                worker_instance.clone(),
                Arc::new(crate::reclaim::ResourceStore(store.clone())),
            );
            interactions
                .register_driver(worker_instance.clone(), Arc::clone(&driver))
                .await;
            if let Some(node) = node.upgrade() {
                node.instance_drivers
                    .write()
                    .await
                    .insert(worker_instance.clone(), Arc::clone(&driver));
            }
            let result = std::panic::AssertUnwindSafe(materialize_instance(
                store.clone(),
                worker_instance.clone(),
                driver,
                receiver,
                interactions,
                tty,
                create_command,
                initial_prompt,
                data_dir,
                objects,
                carrier,
                pumps,
            ))
            .catch_unwind()
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(%error, "instance task exited");
                    record_task_exit(store.as_ref(), &worker_instance, "driver-task-exited");
                }
                Err(_) => {
                    record_task_exit(store.as_ref(), &worker_instance, "driver-task-panicked")
                }
            }
            if store
                .get_instance(&worker_instance)
                .is_ok_and(|instance| instance.lifecycle == InstanceLifecycle::Exited)
                && let Some(node) = node.upgrade()
            {
                node.senders.write().await.remove(&worker_instance);
            }
        });
        self.inner.workers.lock().await.insert(instance_id, worker);
    }

    pub(crate) async fn adopt_worker(&self, instance_id: InstanceId, driver: Arc<dyn Driver>) {
        let (sender, receiver) = mpsc::channel(self.inner.queue_capacity);
        self.inner
            .senders
            .write()
            .await
            .insert(instance_id.clone(), sender);
        self.inner
            .instance_drivers
            .write()
            .await
            .insert(instance_id.clone(), driver.clone());
        self.inner
            .interactions
            .register_driver(instance_id.clone(), driver.clone())
            .await;
        let store = self.inner.store.clone();
        let interactions = Arc::clone(&self.inner.interactions);
        let data_dir = self.data_dir();
        let objects = self.inner.objects.read().ok().and_then(|slot| slot.clone());
        let loader = AttachmentLoader::new(objects, data_dir.clone());
        let id = instance_id.clone();
        let carrier = self.carrier_supervisor();
        // An adopted instance starts with no live prompt correlations: pending
        // registrations are in-memory and die with the Node that owned them.
        let prompts = Arc::new(crate::prompt_correlation::PromptCorrelator::new());
        let node = Arc::downgrade(&self.inner);
        let worker = tokio::spawn(async move {
            if let Err(error) = instance_worker(
                store.clone(),
                id.clone(),
                driver,
                receiver,
                interactions,
                carrier,
                loader,
                prompts,
            )
            .await
            {
                tracing::error!(%error, "adopted instance task exited");
                record_task_exit(store.as_ref(), &id, "driver-task-exited");
            }
            drop_attachments(data_dir.as_deref(), &id);
            if store
                .get_instance(&id)
                .is_ok_and(|instance| instance.lifecycle == InstanceLifecycle::Exited)
                && let Some(node) = node.upgrade()
            {
                node.senders.write().await.remove(&id);
            }
        });
        self.inner.workers.lock().await.insert(instance_id, worker);
    }

    async fn enqueue_existing(
        &self,
        instance_id: &InstanceId,
        command_id: CommandId,
        request: DriverRequest,
        close_after: bool,
        attachment_refs: Vec<remuda_protocol::hubnode::AttachmentRef>,
    ) -> Result<Command, NodeError> {
        let sender = self
            .inner
            .senders
            .read()
            .await
            .get(instance_id)
            .cloned()
            .ok_or(NodeError::DriverUnavailable)?;
        let permit = sender.try_reserve_owned().map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => NodeError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => NodeError::DriverUnavailable,
        })?;
        let mut command = self.inner.store.get_command(&command_id)?;
        accept_command(&mut command)?;
        self.inner.store.save_command(command.clone())?;
        append_command_lifecycle(self.inner.store.as_ref(), instance_id, &command, "accepted")?;
        permit.send(QueuedCommand {
            command_id,
            request,
            close_after,
            attachment_refs,
        });
        if close_after && !pty_queue::is_pty(self.inner.store.get_instance(instance_id)?.driver) {
            self.inner.senders.write().await.remove(instance_id);
        }
        Ok(command)
    }
}

fn spawn_observation_pump(
    store: Arc<dyn LocalStore>,
    interactions: Arc<InteractionRuntime>,
    instance_id: InstanceId,
    mut observations: mpsc::Receiver<remuda_protocol::Observation>,
    driver: Arc<dyn Driver>,
    prompts: Arc<crate::prompt_correlation::PromptCorrelator>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // D-028 §7: `MessageDisplay` deltas are the only live text an
        // agent-in-PTY session has. Folding them here — beside the journal
        // append, not in a path of their own — is what makes the 结构 view
        // fill in line by line instead of a whole message at a time.
        let mut assembler = crate::signal_messages::MessageAssembler::new();
        let mut promoted_hooks = crate::signal::PromotedHooks::default();
        // r-wfprod: hook-triggered Workflow run producer with a 250 ms file
        // drain tick.
        let mut workflow = crate::workflow_producer::WorkflowProducer::new(instance_id.clone());
        let mut workflow_tick = tokio::time::interval(crate::workflow_producer::WORKFLOW_POLL);
        workflow_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                maybe_observation = observations.recv() => {
                    let Some(observation) = maybe_observation else { break };
                    pump_one_observation(
                        &store,
                        &interactions,
                        &instance_id,
                        &driver,
                        &prompts,
                        &mut assembler,
                        &mut promoted_hooks,
                        &mut workflow,
                        observation,
                    ).await;
                }
                _ = workflow_tick.tick() => {
                    for derived in workflow.poll() {
                        if let Err(error) =
                            store.append_driver_observation(&instance_id, derived)
                        {
                            tracing::error!(%error, "workflow observation commit failed");
                        }
                    }
                }
            }
        }
    })
}

/// Fold and commit one driver observation, then any Workflow observations its
/// hook trigger synthesized.
#[allow(clippy::too_many_arguments)]
async fn pump_one_observation(
    store: &Arc<dyn LocalStore>,
    interactions: &Arc<InteractionRuntime>,
    instance_id: &InstanceId,
    driver: &Arc<dyn Driver>,
    prompts: &Arc<crate::prompt_correlation::PromptCorrelator>,
    assembler: &mut crate::signal_messages::MessageAssembler,
    promoted_hooks: &mut crate::signal::PromotedHooks,
    workflow: &mut crate::workflow_producer::WorkflowProducer,
    mut observation: remuda_protocol::Observation,
) {
    if observation.source.driver_kind == remuda_protocol::DriverKind::ShellPty
        && observation.source.channel == remuda_protocol::SourceChannel::Hook
        && !store.get_instance(instance_id).is_ok_and(|instance| {
            // This receiver already belongs to the Node instance.
            // Drivers mint local envelope ids; the store replaces
            // them with Node identity when committing the journal.
            observation.process_generation == instance.process_ref.process_generation
        })
    {
        tracing::warn!("stale hook observation ignored");
        return;
    }
    // C2: stamp this observation with the command that delivered its prompt,
    // if any. Runs after the stale-hook drop so foreign events are never
    // attributed; natively typed prompts match nothing and pass through.
    prompts.correlate(&mut observation);
    // §5.5 before the generic failure fold: a clean exit carries
    // `Severity::Info` and would otherwise fall through both, leaving
    // a finished instance reported as `ready`.
    if let Some(exit) = native_exit(&observation) {
        record_native_exit(store.as_ref(), instance_id, &exit);
    } else if let Some(reason) = native_failure_reason(&observation) {
        record_task_exit(store.as_ref(), instance_id, &reason);
    }
    if let Some(promotion) = promotion_change(&observation) {
        if let Err(error) = store.set_instance_promotion(
            instance_id,
            promotion.kind,
            promotion.mode,
            promotion.promoted_at,
        ) {
            tracing::warn!(%error, "instance promotion not applied");
        } else {
            // Promotion is precisely the event that changes the
            // answer: the PTY was carrying a login shell and is now
            // carrying an agent, so steer / queue / interrupt move
            // from "not provided" to that harness's measured
            // provisions (§4.3, §6).
            refresh_capabilities(store.as_ref(), instance_id, driver.as_ref()).await;
        }
    }
    let promoted_hook = observation.source.driver_kind == remuda_protocol::DriverKind::ShellPty;
    let bound = promoted_hooks.observe(&observation);
    if let Some(session) = bound {
        record_native_session(
            store.as_ref(),
            instance_id,
            &NativeSessionEvidence {
                session_id: session.session_id,
                transcript_path: session.transcript_path,
                signal_tier: Some(remuda_protocol::SignalTier::Hook),
            },
        );
    } else if !(promoted_hook && observation.source.channel == remuda_protocol::SourceChannel::Hook)
        && let Some(session) = native_session_evidence(&observation)
    {
        record_native_session(store.as_ref(), instance_id, &session);
    }
    // D-028 §4.3: Hook outranks File, and both outrank Screen. Without
    // this the hook/file events are journaled but the instance still
    // follows `agent_status`, which is a screen guess — the composer
    // would keep believing the screen over the harness's own account.
    //
    // Promoted hand-typed sessions fold through the promoted-hook
    // tracker; a launched session folds through the static classifier.
    let hook_activity = if promoted_hook {
        promoted_hooks.activity(&observation)
    } else {
        crate::signal::hook_activity(&observation)
    };
    let mut activity_annotated = false;
    if promoted_hook
        && let ObservationPayload::Lifecycle(payload) = &mut observation.body
        && let LifecyclePayload::Native(native) = payload.as_mut()
    {
        // This annotation belongs to the Node, never to the harness.
        // Carry validated state on its causal event so Hub/web need
        // not wait for a second serialized journal append/ACK.
        native.related_ids.remove("remudaActivity");
        let activity = match hook_activity {
            Some(Activity::Working) => Some("working"),
            Some(Activity::Idle) => Some("idle"),
            _ => None,
        };
        if let Some(activity) = activity {
            native
                .related_ids
                .insert("remudaActivity".into(), activity.into());
            activity_annotated = true;
        }
    }
    // P6: File turn lifecycles fold only when no hook set activity and
    // only for a kind with a file-tail adapter (codex/grok); the
    // registry is the single lookup rather than another per-kind branch.
    let activity = hook_activity.or_else(|| match store.get_instance(instance_id) {
        Ok(instance) if crate::adapter_registry::has_file_adapter(instance.kind) => {
            crate::signal::file_activity(&observation)
        }
        _ => None,
    });
    if let Some(activity) = activity
        && let Err(error) = store.set_instance_state(
            instance_id,
            None,
            Some(remuda_protocol::Knowledge::Known { value: activity }),
        )
    {
        tracing::warn!(%error, "hook/file activity not applied");
    }
    match store.append_driver_observation(instance_id, observation) {
        Ok(committed) => {
            if let Err(error) = interactions.ingest(&committed).await {
                tracing::debug!(%error, "interaction ingest failed");
            }
            // The hook event itself is the evidence and is journaled
            // above; this is the readable message derived from it. It
            // follows the hook so the two stay in seq order, and is
            // built from `committed` so it inherits the envelope the
            // store just stamped. Unbound/foreign shell hooks remain
            // raw evidence; only the verified foreground can add text.
            if (!promoted_hook || promoted_hooks.owns_hook(&committed))
                && let Some(delta) = crate::signal_messages::message_delta(&committed)
                && let Some(payload) = assembler.fold(&delta)
            {
                let message =
                    crate::signal_messages::MessageAssembler::observation(&committed, payload);
                if let Err(error) = store.append_driver_observation(instance_id, message) {
                    tracing::warn!(%error, "streamed message not journaled");
                }
            }
            if hook_activity.is_some() && !activity_annotated {
                record_hook_activity(store.as_ref(), instance_id);
            }
            // r-wfprod: synthesize Workflow card observations from
            // this hook trigger (launch snapshot / member start) and
            // append them immediately so the latency budgets hold.
            for derived in workflow.on_observation(&committed) {
                if let Err(error) = store.append_driver_observation(instance_id, derived) {
                    tracing::warn!(%error, "workflow observation not journaled");
                }
            }
        }
        Err(error) => {
            tracing::error!(%error, instance_id = %instance_id.as_id(), "native observation commit failed");
            record_task_exit(
                store.as_ref(),
                instance_id,
                "native-observation-commit-failed",
            );
        }
    }
}

async fn materialize_instance(
    store: Arc<dyn LocalStore>,
    instance_id: InstanceId,
    driver: Arc<dyn Driver>,
    mut receiver: mpsc::Receiver<QueuedCommand>,
    interactions: Arc<InteractionRuntime>,
    tty: TtyRegistry,
    mut create_command: Command,
    initial_prompt: String,
    // Node data dir, so this worker can drop its attachments on the way out.
    data_dir: Option<std::path::PathBuf>,
    // Hub object source, for worker-side attachment pulls (D-027).
    objects: Option<Arc<dyn crate::attachments::ObjectSource>>,
    carrier: Option<crate::carrier_recovery::CarrierSupervisor>,
    // Where this instance's observation pump is registered so a shutdown can
    // stop it. Just the map, not the whole Node: the pump outliving its Node is
    // the bug being fixed, and handing this worker a Node handle to fix it
    // would be a second way to do the same thing.
    pumps: Arc<tokio::sync::Mutex<BTreeMap<InstanceId, tokio::task::JoinHandle<()>>>>,
) -> Result<(), NodeError> {
    // C2: per-instance registry joining command-delivered prompts onto the
    // hook/transcript observations that confirm them.
    let prompts = Arc::new(crate::prompt_correlation::PromptCorrelator::new());
    // Protocol §2.3: build is done; the dispatch intent is durable and the
    // native process is about to be spawned. Journal `preparing → starting`
    // before touching the driver so a Hub that never got the RPC reply still
    // converges this instance instead of leaving it at `requested`.
    journal_instance_phase(
        store.as_ref(),
        &instance_id,
        InstanceLifecycle::Starting,
        "preparing",
        "driver-spawn",
    )?;
    let start_span = tracing::info_span!("driver.start");
    let start_result = {
        use tracing::Instrument;
        driver.start().instrument(start_span).await
    };
    let observations = match start_result {
        Ok(observations) => observations,
        Err(error) => {
            notify_carrier(carrier.as_ref(), &error);
            reject_materialization(
                store.as_ref(),
                &instance_id,
                &mut create_command,
                &mut receiver,
                &error.to_string(),
            )
            .await?;
            return Ok(());
        }
    };
    if let Some(error) = driver.startup_error() {
        if let Err(cleanup) = driver.execute(DriverRequest::Close).await {
            tracing::error!(%cleanup, "startup cleanup failed");
        }
        if let Some(mut pending) = observations {
            while let Ok(observation) = pending.try_recv() {
                let _ = store.append_driver_observation(&instance_id, observation);
            }
        }
        reject_materialization(
            store.as_ref(),
            &instance_id,
            &mut create_command,
            &mut receiver,
            &error,
        )
        .await?;
        return Ok(());
    }
    if let Some(observations) = observations {
        let pump = spawn_observation_pump(
            Arc::clone(&store),
            Arc::clone(&interactions),
            instance_id.clone(),
            observations,
            Arc::clone(&driver),
            Arc::clone(&prompts),
        );
        pumps.lock().await.insert(instance_id.clone(), pump);
    }
    if let Some(recipe) = driver.launch_recipe() {
        store.put_launch_recipe(&instance_id, &recipe)?;
    }
    if let Some(bridge) = driver.tty_bridge().await
        && let Err(error) = tty
            .start(
                instance_id.clone(),
                bridge,
                crate::TTY_DEFAULT_COLS,
                crate::TTY_DEFAULT_ROWS,
            )
            .await
    {
        tracing::warn!(
            %error,
            instance_id = %instance_id.as_id(),
            "tty bridge failed to start"
        );
    }
    // §2.3: native initialization finished; the session owns a running PTY.
    // Set the entity first, then journal it, so readers that observe `ready`
    // find the event explaining it.
    journal_instance_phase(
        store.as_ref(),
        &instance_id,
        InstanceLifecycle::Ready,
        "starting",
        "driver-started",
    )?;
    // D-028 §4.3/§6: the create-time snapshot is keyed by `DriverKind` and
    // cannot know what this session is actually carrying. Ask the live driver
    // now that it has started, so the wire reports the session's real
    // steer / queue / interrupt provisions instead of the static row.
    refresh_capabilities(store.as_ref(), &instance_id, driver.as_ref()).await;

    let loader = AttachmentLoader::new(objects, data_dir.clone());
    if pty_queue::is_pty(driver.kind()) {
        let result = pty_queue::run(
            store,
            instance_id.clone(),
            driver,
            receiver,
            interactions,
            Some((create_command, initial_prompt)),
            carrier,
            loader,
            Arc::clone(&prompts),
        )
        .await;
        tty.stop(&instance_id).await;
        drop_attachments(data_dir.as_deref(), &instance_id);
        return result;
    }
    if initial_prompt.is_empty() {
        settle_without_driver(store.as_ref(), &instance_id, &mut create_command)?;
    } else {
        execute_queued(
            Arc::clone(&store),
            &instance_id,
            Arc::clone(&driver),
            QueuedCommand {
                command_id: create_command.command_id.clone(),
                request: DriverRequest::Send {
                    prompt: initial_prompt,
                    // `instance.create` stages no attachments; the first image
                    // arrives on a later send.
                    attachments: Vec::new(),
                    origin: crate::origin::input_origin(create_command.origin),
                },
                close_after: false,
                attachment_refs: Vec::new(),
            },
            Arc::clone(&interactions),
            carrier.clone(),
            loader.clone(),
            Arc::clone(&prompts),
        )
        .await?;
    }

    let result = instance_worker(
        store,
        instance_id.clone(),
        driver,
        receiver,
        interactions,
        carrier,
        loader,
        prompts,
    )
    .await;
    tty.stop(&instance_id).await;
    drop_attachments(data_dir.as_deref(), &instance_id);
    result
}

/// Remove an instance's materialized attachments once its worker is done
/// (D-027). Best effort: a failure here must not mask the worker's own result,
/// and `sweep_attachment_orphans` catches whatever a hard kill leaves behind.
fn drop_attachments(data_dir: Option<&std::path::Path>, instance_id: &InstanceId) {
    let Some(data_dir) = data_dir else {
        return;
    };
    if let Err(error) = crate::attachments::cleanup(data_dir, instance_id) {
        tracing::warn!(
            instance_id = %instance_id.as_id(),
            %error,
            "could not remove instance attachments"
        );
    }
}

async fn reject_materialization(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    create_command: &mut Command,
    receiver: &mut mpsc::Receiver<QueuedCommand>,
    reason: &str,
) -> Result<(), NodeError> {
    settle_command(
        create_command,
        SettlementOutcome::Rejected,
        Some(reason.to_owned()),
        remuda_protocol::ExecutionState::NotDispatched,
    )?;
    store.save_command(create_command.clone())?;
    append_command_lifecycle(store, instance_id, create_command, "settled")?;

    receiver.close();
    while let Some(queued) = receiver.recv().await {
        let mut command = store.get_command(&queued.command_id)?;
        settle_command(
            &mut command,
            SettlementOutcome::Rejected,
            Some("instance materialization failed".to_owned()),
            remuda_protocol::ExecutionState::NotDispatched,
        )?;
        store.save_command(command.clone())?;
        append_command_lifecycle(store, instance_id, &command, "settled")?;
    }
    record_task_exit(store, instance_id, reason);
    Ok(())
}

async fn instance_worker(
    store: Arc<dyn LocalStore>,
    instance_id: InstanceId,
    driver: Arc<dyn Driver>,
    mut receiver: mpsc::Receiver<QueuedCommand>,
    interactions: Arc<InteractionRuntime>,
    carrier: Option<crate::carrier_recovery::CarrierSupervisor>,
    loader: AttachmentLoader,
    prompts: Arc<crate::prompt_correlation::PromptCorrelator>,
) -> Result<(), NodeError> {
    if pty_queue::is_pty(driver.kind()) {
        return pty_queue::run(
            store,
            instance_id,
            driver,
            receiver,
            interactions,
            None,
            carrier,
            loader,
            prompts,
        )
        .await;
    }
    while let Some(queued) = receiver.recv().await {
        let close_after = queued.close_after;
        execute_queued(
            Arc::clone(&store),
            &instance_id,
            Arc::clone(&driver),
            queued,
            Arc::clone(&interactions),
            carrier.clone(),
            loader.clone(),
            Arc::clone(&prompts),
        )
        .await?;
        if close_after {
            return Ok(());
        }
    }
    Err(NodeError::DriverUnavailable)
}

async fn execute_queued(
    store: Arc<dyn LocalStore>,
    instance_id: &InstanceId,
    driver: Arc<dyn Driver>,
    mut queued: QueuedCommand,
    interactions: Arc<InteractionRuntime>,
    carrier: Option<crate::carrier_recovery::CarrierSupervisor>,
    loader: AttachmentLoader,
    prompts: Arc<crate::prompt_correlation::PromptCorrelator>,
) -> Result<(), NodeError> {
    // D-027: pull bytes in the worker, after the command was durably accepted.
    // A fetch failure settles just this command (rejected, not dispatched)
    // rather than killing the instance.
    let queued = if let DriverRequest::Send { attachments, .. } = &mut queued.request {
        match loader
            .resolve(instance_id, queued.attachment_refs.clone())
            .await
        {
            Ok(materialized) => {
                *attachments = materialized;
                queued
            }
            Err(error) => {
                let mut command = store.get_command(&queued.command_id)?;
                settle_command(
                    &mut command,
                    SettlementOutcome::Rejected,
                    Some(error.to_string()),
                    remuda_protocol::ExecutionState::NotDispatched,
                )?;
                store.save_command(command.clone())?;
                append_command_lifecycle(store.as_ref(), instance_id, &command, "settled")?;
                return Ok(());
            }
        }
    } else {
        queued
    };
    let mut command = store.get_command(&queued.command_id)?;
    if let DriverRequest::Send {
        prompt,
        attachments,
        ..
    } = &queued.request
    {
        store.set_instance_state(
            instance_id,
            None,
            Some(Knowledge::Known {
                value: Activity::Working,
            }),
        )?;
        // C2: the non-PTY synthesized user message carries the delivering
        // command id, and stdout/transcript user frames join its node the same
        // way the PTY transcript does — one user node per command, everywhere.
        // D-027b: it also carries the landed image/file blocks, so the
        // absolute landed path is recorded as journal metadata.
        let mut payload = crate::driver::message_payload(
            MessageRole::User,
            MessagePhase::Input,
            prompt.clone(),
            crate::attachments::content_blocks(attachments),
        )?;
        if let ObservationPayload::Message(message) = &mut payload {
            message.command_id = Some(queued.command_id.clone());
            prompts.register(
                queued.command_id.clone(),
                message.mutation.node_id.clone(),
                prompt.clone(),
            );
        }
        store.append_observation(instance_id, None, Completeness::Structured, payload)?;
        if let Err(error) = driver.wait_control().await {
            prompts.cancel(&queued.command_id);
            notify_carrier(carrier.as_ref(), &error);
            let diagnostic = DriverEmission::NativeLifecycle {
                name: "control-wait".to_owned(),
                status: error.to_string(),
                severity: remuda_protocol::Severity::Error,
            };
            store.append_observation(
                instance_id,
                None,
                Completeness::Structured,
                diagnostic.into_payload()?,
            )?;
            settle_command(
                &mut command,
                SettlementOutcome::Rejected,
                Some(error.to_string()),
                remuda_protocol::ExecutionState::PossiblyDispatched,
            )?;
            store.save_command(command.clone())?;
            append_command_lifecycle(store.as_ref(), instance_id, &command, "settled")?;
            return Ok(());
        }
    }

    let execution = match &queued.request {
        DriverRequest::RespondInteraction {
            interaction_id,
            answer,
        } => interactions
            .dispatch_rpc(
                "interaction.answer",
                serde_json::json!({
                    "interactionId": interaction_id,
                    "answer": answer,
                    "commandId": command.command_id.as_id().as_str(),
                }),
            )
            .await
            .map(|_| Vec::new())
            .map_err(|error| crate::DriverError::Failed(error.to_string())),
        request => driver.execute(request.clone()).await,
    };
    match execution {
        Ok(emissions) => {
            for emission in emissions {
                let observation = store.append_observation(
                    instance_id,
                    None,
                    Completeness::Structured,
                    emission.into_payload()?,
                )?;
                interactions.ingest(&observation).await?;
            }
            finish_instance_operation(store.as_ref(), instance_id, &queued.request)?;
            settle_command(
                &mut command,
                SettlementOutcome::Completed,
                None,
                remuda_protocol::ExecutionState::PossiblyDispatched,
            )?;
        }
        Err(error) => {
            // A lost carrier is the Node's problem to fix, not the command's.
            // Recovery runs in the background; this command still settles now.
            notify_carrier(carrier.as_ref(), &error);
            // §9.1: an effort switch a carrier cannot do (claude-print) is an
            // honest capability rejection, not a driver fault — name it for the
            // operation so the UI can show "unsupported in this session".
            let diagnostic_name = if matches!(queued.request, DriverRequest::Configure { .. }) {
                "instance.configure"
            } else {
                "fake-driver-error"
            };
            let diagnostic = DriverEmission::NativeLifecycle {
                name: diagnostic_name.to_owned(),
                status: format!("rejected: {error}"),
                severity: remuda_protocol::Severity::Warning,
            };
            store.append_observation(
                instance_id,
                None,
                Completeness::Structured,
                diagnostic.into_payload()?,
            )?;
            // A control operation's rejection (e.g. an effort switch a carrier
            // cannot do) must not mark the instance itself failed.
            let is_control = matches!(queued.request, DriverRequest::Configure { .. });
            if !pty_queue::is_pty(driver.kind()) && !is_control {
                record_task_exit(store.as_ref(), instance_id, &error.to_string());
            }
            settle_command(
                &mut command,
                SettlementOutcome::Rejected,
                Some(error.to_string()),
                remuda_protocol::ExecutionState::PossiblyDispatched,
            )?;
        }
    }
    store.save_command(command.clone())?;
    append_command_lifecycle(store.as_ref(), instance_id, &command, "settled")
}

/// Ask the Node to restart a lost Herdr session server, if that is what this
/// error means. Never blocks the command that hit it.
fn notify_carrier(
    carrier: Option<&crate::carrier_recovery::CarrierSupervisor>,
    error: &crate::DriverError,
) {
    if let Some(carrier) = carrier {
        carrier.on_driver_error(error);
    }
}

fn finish_instance_operation(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    request: &DriverRequest,
) -> Result<(), NodeError> {
    match request {
        DriverRequest::Close => {
            store.set_instance_state(
                instance_id,
                Some(InstanceLifecycle::Exited),
                Some(Knowledge::Known {
                    value: Activity::Idle,
                }),
            )?;
            append_instance_lifecycle(
                store,
                instance_id,
                Some("ready"),
                "exited",
                "explicit-close",
            )?;
        }
        DriverRequest::Send { .. }
        | DriverRequest::Cancel
        | DriverRequest::SendKeys { .. }
        | DriverRequest::Configure { .. } => {
            store.set_instance_state(
                instance_id,
                None,
                Some(Knowledge::Known {
                    value: Activity::Idle,
                }),
            )?;
        }
        DriverRequest::RespondInteraction { .. } => {}
    }
    Ok(())
}

fn record_task_exit(store: &dyn LocalStore, instance_id: &InstanceId, reason: &str) {
    if store
        .get_instance(instance_id)
        .is_ok_and(|i| i.lifecycle == InstanceLifecycle::Exited)
    {
        return;
    }
    if let Err(error) = store.mark_unsettled_unknown(instance_id) {
        tracing::error!(%error, "failed to mark commands unknown after task exit");
    }
    if let Err(error) = store.set_instance_failure(instance_id, reason) {
        tracing::error!(%error, "failed to mark instance failed after task exit");
        return;
    }
    if let Err(error) =
        append_instance_lifecycle(store, instance_id, Some("ready"), "failed", reason)
    {
        tracing::error!(%error, "failed to append task-exit lifecycle event");
    }
}

/// Store what the live driver says this session can do (§4.3, §6).
///
/// Called after start and again after a promotion, because promotion is
/// exactly the event that changes the answer: a `shell-pty` that was carrying a
/// login shell is now carrying `claude`, and steer / queue / interrupt go from
/// "not provided" to the provisions measured for that harness.
///
/// Silent when the driver has nothing to add — a driver whose abilities really
/// are fixed by its kind returns `None` and keeps its create-time snapshot.
async fn refresh_capabilities(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    driver: &dyn Driver,
) {
    let Some(snapshot) = driver.capabilities().await else {
        return;
    };
    match store.set_instance_capabilities(instance_id, snapshot) {
        Ok(Some(instance)) => tracing::debug!(
            instance = %instance_id.as_id(),
            revision = instance.meta.revision.0,
            "session capabilities refreshed from the live driver"
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "session capabilities not stored"),
    }
}

/// A terminal → agent promotion or demotion read off a driver lifecycle (D-025).
struct PromotionChange {
    kind: AgentKind,
    mode: remuda_protocol::InstanceMode,
    promoted_at: Option<remuda_protocol::Timestamp>,
}

/// Recognize the driver's `agent_promoted` / `agent_demoted` lifecycle.
///
/// Only these two names move `kind`. The accompanying `agent_detected`
/// diagnostic is journal-only: it explains *why* to a human reading the
/// journal, and must not be a second path into the entity.
fn promotion_change(observation: &remuda_protocol::Observation) -> Option<PromotionChange> {
    let ObservationPayload::Lifecycle(payload) = &observation.body else {
        return None;
    };
    let LifecyclePayload::Native(native) = payload.as_ref() else {
        return None;
    };
    let mode = match native.native_name.as_str() {
        "agent_promoted" => remuda_protocol::InstanceMode::Promoted,
        "agent_demoted" => remuda_protocol::InstanceMode::Native,
        _ => return None,
    };
    let kind = agent_kind(native.related_ids.get("kind")?)?;
    let promoted_at = native
        .related_ids
        .get("promotedAt")
        .filter(|_| mode == remuda_protocol::InstanceMode::Promoted)
        .and_then(|value| remuda_protocol::Timestamp::try_from(value.clone()).ok());
    Some(PromotionChange {
        kind,
        mode,
        promoted_at,
    })
}

fn agent_kind(wire: &str) -> Option<AgentKind> {
    match wire {
        "claude" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        "grok" => Some(AgentKind::Grok),
        "agy" => Some(AgentKind::Agy),
        "generic" => Some(AgentKind::Generic),
        "terminal" => Some(AgentKind::Terminal),
        _ => None,
    }
}

/// Native session identity carried by a driver lifecycle observation (D-026).
struct NativeSessionEvidence {
    session_id: String,
    transcript_path: Option<String>,
    signal_tier: Option<remuda_protocol::SignalTier>,
}

/// Read the real Claude session id out of a driver observation.
///
/// `claude-print` reports it on the `session` lifecycle mapped from stream-json
/// `system/init`; `claude-pty` reports it (with the transcript path) from its
/// `SessionStart` hook. Both are the only proof Remuda has of the identity
/// `claude --resume` will accept, so `nativeRef` must be corrected from them
/// instead of keeping the Instance id minted at create time.
fn native_session_evidence(
    observation: &remuda_protocol::Observation,
) -> Option<NativeSessionEvidence> {
    let ObservationPayload::Lifecycle(payload) = &observation.body else {
        return None;
    };
    let LifecyclePayload::Native(native) = payload.as_ref() else {
        return None;
    };
    // `nativeId` means different things per topic, so match the two events
    // that actually carry a session: claude-print's `session` lifecycle, and
    // claude-pty's SessionStart hook. Claude-print also emits hook lifecycles
    // whose nativeId is a *hook* id — accepting those overwrote the real
    // session with an id `--resume` rejects.
    let transcript_path = native
        .related_ids
        .get("transcriptPath")
        .map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty());
    let carries_session = match native.topic {
        // `transcript_bound` is the promoted terminal's deterministic claim.
        remuda_protocol::LifecycleTopic::Session => {
            matches!(native.native_name.as_str(), "session" | "transcript_bound")
        }
        remuda_protocol::LifecycleTopic::Hook => {
            native.native_name == "SessionStart" && transcript_path.is_some()
        }
        _ => false,
    };
    if !carries_session {
        return None;
    }
    let session_id = match &native.native_id {
        Knowledge::Known { value } if !value.trim().is_empty() => value.trim().to_owned(),
        _ => return None,
    };
    Some(NativeSessionEvidence {
        session_id,
        transcript_path,
        // Legacy claude-pty registers SessionStart for identity only and still
        // derives turn activity from its screen. Only the authenticated
        // promoted-hook fold proves the complete turn-hook path above.
        signal_tier: None,
    })
}

/// Persist observed session identity and journal it once per change.
fn record_native_session(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    evidence: &NativeSessionEvidence,
) {
    let updated = match store.set_native_session(
        instance_id,
        &evidence.session_id,
        evidence.transcript_path.as_deref(),
        evidence.signal_tier,
    ) {
        Ok(Some(instance)) => instance,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, instance_id = %instance_id.as_id(), "native session record failed");
            return;
        }
    };
    let state = serde_json::to_value(updated.lifecycle)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "ready".to_owned());
    if let Err(error) = append_instance_lifecycle(
        store,
        instance_id,
        Some(&state),
        &state,
        "native-session-recorded",
    ) {
        tracing::warn!(%error, "native session lifecycle append failed");
    }
}

fn record_hook_activity(store: &dyn LocalStore, instance_id: &InstanceId) {
    let result = store.get_instance(instance_id).and_then(|instance| {
        let state = serde_json::to_value(instance.lifecycle)?;
        append_instance_lifecycle(
            store,
            instance_id,
            None,
            state.as_str().unwrap_or("ready"),
            "hook-activity",
        )
    });
    if let Err(error) = result {
        tracing::warn!(%error, "hook activity lifecycle append failed");
    }
}

fn native_failure_reason(observation: &remuda_protocol::Observation) -> Option<String> {
    let ObservationPayload::Lifecycle(payload) = &observation.body else {
        return None;
    };
    let LifecyclePayload::Native(native) = payload.as_ref() else {
        return None;
    };
    let name = native.native_name.to_ascii_lowercase();
    let failed = native.severity == remuda_protocol::Severity::Error
        || name.contains("error")
        || name == "exit"
        || name.contains("gone")
        || name.contains("agent_not_ready")
        || name.contains("shell");
    if !failed {
        return None;
    }
    if let Some(message) = native.related_ids.get("lastError")
        && !message.is_empty()
    {
        return Some(message.clone());
    }
    match &native.status {
        Knowledge::Known { value } if !value.is_empty() => Some(value.clone()),
        _ => Some(native.native_name.clone()),
    }
}

/// A PTY process that ended, as reported by the driver's exit waiter (§5.5).
///
/// Separate from [`native_failure_reason`] because a *clean* exit is not a
/// failure and must not be journaled as one: the `Severity::Info` a code-0 exit
/// carries is exactly what keeps it out of that path, and without this it would
/// be journaled and then silently dropped, leaving the instance `ready` — which
/// is the §5.5 defect in a new place rather than a fix for it.
struct NativeExit {
    /// `exited` or `failed`.
    state: String,
    /// Reason naming the evidence, e.g. `native-exit-code-0`.
    reason: String,
}

fn native_exit(observation: &remuda_protocol::Observation) -> Option<NativeExit> {
    let ObservationPayload::Lifecycle(payload) = &observation.body else {
        return None;
    };
    let LifecyclePayload::Native(native) = payload.as_ref() else {
        return None;
    };
    if native.native_name != remuda_driver::shell_pty::NATIVE_EXIT {
        return None;
    }
    let Knowledge::Known { value: state } = &native.status else {
        return None;
    };
    Some(NativeExit {
        state: state.clone(),
        reason: native
            .related_ids
            .get("reason")
            .cloned()
            .unwrap_or_else(|| state.clone()),
    })
}

/// Settle an instance whose PTY process ended (§5.5).
///
/// §5.5's other half: a *promoted* instance distinguishes two deaths. The agent
/// going away while the shell survives is a demote, and the driver reports that
/// as `agent_demoted`, never as this event — the exit waiter watches the child
/// the driver itself spawned. So anything arriving here really is the instance
/// ending.
fn record_native_exit(store: &dyn LocalStore, instance_id: &InstanceId, exit: &NativeExit) {
    let Ok(instance) = store.get_instance(instance_id) else {
        return;
    };
    if matches!(
        instance.lifecycle,
        InstanceLifecycle::Exited | InstanceLifecycle::Failed
    ) {
        return;
    }
    if let Err(error) = store.mark_unsettled_unknown(instance_id) {
        tracing::warn!(%error, "unsettled commands not marked before native exit");
    }
    if exit.state == "failed"
        && let Err(error) = store.set_instance_failure(instance_id, &exit.reason)
    {
        tracing::warn!(%error, "native exit failure reason not recorded");
    }
    // Journal first, settle second. A reader that sees `exited` will then
    // always find the event explaining it: the other order leaves a window in
    // which an instance has stopped and the journal cannot say why, and a UI
    // polling for the transition lands in that window under load. Ordering it
    // this way makes the entity state the *last* thing to change, so it is
    // safe to treat as the signal that everything else is already written.
    if let Err(error) =
        append_instance_lifecycle(store, instance_id, Some("ready"), &exit.state, &exit.reason)
    {
        tracing::error!(%error, "native exit lifecycle not appended");
    }
    if let Err(error) = store.set_instance_state(
        instance_id,
        Some(InstanceLifecycle::Exited),
        Some(Knowledge::Known {
            value: Activity::Idle,
        }),
    ) {
        tracing::error!(%error, "instance not marked exited after its process ended");
    }
}

fn command_parts(
    request: &InstanceCommandRequest,
    attachments: Vec<crate::attachments::MaterializedAttachment>,
) -> Result<(CommandOperation, DriverRequest, bool), NodeError> {
    match request.operation {
        CommandAction::Send => {
            let prompt = request
                .prompt
                .clone()
                .ok_or_else(|| NodeError::InvalidRequest("send requires prompt".to_owned()))?;
            validate_text(&prompt, "prompt")?;
            Ok((
                CommandOperation::InstanceSend,
                DriverRequest::Send {
                    prompt,
                    attachments,
                    origin: request.origin,
                },
                false,
            ))
        }
        CommandAction::Cancel => Ok((
            CommandOperation::InstanceCancel,
            DriverRequest::Cancel,
            false,
        )),
        CommandAction::RespondInteraction => {
            let interaction_id = request.interaction_id.as_ref().ok_or_else(|| {
                NodeError::InvalidRequest("respond_interaction requires interactionId".to_owned())
            })?;
            let answer = request.answer.clone().ok_or_else(|| {
                NodeError::InvalidRequest("respond_interaction requires answer".to_owned())
            })?;
            Ok((
                CommandOperation::InteractionRespond,
                DriverRequest::RespondInteraction {
                    interaction_id: interaction_id.as_id().to_string(),
                    answer,
                },
                false,
            ))
        }
        CommandAction::Close => Ok((CommandOperation::InstanceClose, DriverRequest::Close, true)),
        CommandAction::WriteTty => {
            let keys = request.keys.as_deref().unwrap_or_default();
            if keys.is_empty() {
                return Err(NodeError::InvalidRequest(
                    "tty.write requires keys".to_owned(),
                ));
            }
            let keys = keys
                .iter()
                .map(|key| validate_tty_key(key))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((
                CommandOperation::TtyWrite,
                DriverRequest::SendKeys { keys },
                false,
            ))
        }
        CommandAction::Configure => Ok((
            CommandOperation::InstanceConfigure,
            DriverRequest::Configure {
                model: request.model.clone(),
                effort: request.effort_name.clone(),
                effort_index: request.effort_index,
            },
            false,
        )),
    }
}

// Keep the logical-key vocabulary aligned with the CLI encoder. The Node is
// the trust boundary: raw RPC callers do not pass through that encoder.
fn validate_tty_key(key: &str) -> Result<String, NodeError> {
    let mut chars = key.chars();
    if let Some(ch) = chars.next()
        && chars.next().is_none()
        && !ch.is_control()
        && !ch.is_whitespace()
    {
        return Ok(key.to_owned());
    }
    let name = key.to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "enter"
            | "return"
            | "tab"
            | "esc"
            | "escape"
            | "space"
            | "backspace"
            | "bs"
            | "delete"
            | "del"
            | "up"
            | "down"
            | "left"
            | "right"
            | "home"
            | "end"
            | "c-c"
    ) || (name.starts_with("ctrl+")
        && name.len() == 6
        && name.as_bytes()[5].is_ascii_lowercase())
    {
        return Ok(name);
    }
    Err(NodeError::InvalidRequest(
        "unsupported tty.write key".into(),
    ))
}

fn validate_kind_driver(
    kind: AgentKind,
    driver: remuda_protocol::DriverKind,
) -> Result<(), NodeError> {
    let valid = matches!(
        (kind, driver),
        (AgentKind::Claude, remuda_protocol::DriverKind::ClaudePrint)
            | (AgentKind::Claude, remuda_protocol::DriverKind::ClaudePty)
            | (AgentKind::Claude, remuda_protocol::DriverKind::ClaudeBg)
            | (
                AgentKind::Codex,
                remuda_protocol::DriverKind::CodexAppserver
            )
            | (AgentKind::Grok, remuda_protocol::DriverKind::GrokAcp)
            | (AgentKind::Agy, remuda_protocol::DriverKind::AgyPrint)
            | (AgentKind::Claude, remuda_protocol::DriverKind::GenericPty)
            | (AgentKind::Codex, remuda_protocol::DriverKind::GenericPty)
            | (AgentKind::Grok, remuda_protocol::DriverKind::GenericPty)
            | (AgentKind::Agy, remuda_protocol::DriverKind::GenericPty)
            | (AgentKind::Generic, remuda_protocol::DriverKind::GenericPty)
            | (AgentKind::Terminal, remuda_protocol::DriverKind::ShellPty)
            | (AgentKind::Generic, remuda_protocol::DriverKind::ShellPty)
            // D-028 §5.1: an agent CLI in a Remuda-owned native PTY. This is
            // the same carrier a promoted `terminal` already runs on — the
            // matrix refusing it was what forced New Session down a separate
            // path from "the user typed `claude`", which §1.0 exists to end.
            | (AgentKind::Claude, remuda_protocol::DriverKind::ShellPty)
            | (AgentKind::Codex, remuda_protocol::DriverKind::ShellPty)
            | (AgentKind::Grok, remuda_protocol::DriverKind::ShellPty)
            | (AgentKind::Agy, remuda_protocol::DriverKind::ShellPty)
    );
    if valid {
        Ok(())
    } else {
        Err(NodeError::InvalidRequest(
            "kind and driver do not describe the same native product".to_owned(),
        ))
    }
}

fn validate_text(text: &str, label: &str) -> Result<(), NodeError> {
    if text.len() > 64 * 1024 {
        return Err(NodeError::InvalidRequest(format!(
            "{label} exceeds the 65536-byte local limit"
        )));
    }
    Ok(())
}

fn new_command(
    command_id: CommandId,
    operation: CommandOperation,
    instance_id: &InstanceId,
    host_id: &HostId,
    node_epoch: &Id,
    run_id: Option<remuda_protocol::RunId>,
    payload_digest: WireDigest,
) -> Result<Command, NodeError> {
    let now = timestamp_now()?;
    Ok(Command {
        meta: EntityMeta {
            id: command_id.clone(),
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now.clone(),
        },
        command_id,
        actor: ActorRef {
            principal_id: Id::new("prn")?,
            actor_type: ActorType::Agent,
            device_id: None,
            instance_id: Some(instance_id.clone()),
        },
        origin: CommandOrigin::Mcp,
        operation,
        target: CommandTarget {
            host_id: host_id.clone(),
            instance_id: Some(instance_id.clone()),
            run_id,
        },
        payload_ref: Id::new("obj")?,
        payload_digest,
        expected: ExpectedState {
            instance_revision: None,
            process_generation: Some(U64(1)),
            run_generation: None,
            owner_fence: Some(U64(1)),
            interaction_version: None,
        },
        state: CommandState::Queued,
        authority: CommandAuthority::NodeLedger,
        forward_intent: Knowledge::NotApplicable,
        dispatch: DispatchState::NotDispatched,
        resolution: ResolutionState::Clear,
        acceptance: unknown("not-accepted"),
        settlement: unknown("not-settled"),
        queued_at: now,
        accepted_at: unknown("not-accepted"),
        settled_at: unknown("not-settled"),
        expires_at: None,
        node_receipt: Knowledge::Known {
            value: NodeReceipt {
                node_epoch: node_epoch.clone(),
                ledger_revision: U64(1),
            },
        },
    })
}

fn set_command_origin(command: &mut Command, origin: remuda_protocol::InputOrigin) {
    command.origin = crate::origin::command_origin(origin);
    command.actor.actor_type = match origin {
        remuda_protocol::InputOrigin::Human => ActorType::Human,
        remuda_protocol::InputOrigin::Bot => ActorType::Bot,
        remuda_protocol::InputOrigin::Agent => ActorType::Agent,
    };
}

fn accept_command(command: &mut Command) -> Result<(), NodeError> {
    let now = timestamp_now()?;
    command.state = CommandState::Accepted;
    command.dispatch = DispatchState::IntentDurable;
    command.acceptance = Knowledge::Known {
        value: Acceptance {
            scope: acceptance_scope(command.operation),
            event_ids: Vec::new(),
        },
    };
    command.accepted_at = Knowledge::Known { value: now.clone() };
    command.meta.updated_at = now;
    command.meta.revision.0 = command.meta.revision.0.saturating_add(1);
    Ok(())
}

fn acceptance_scope(operation: CommandOperation) -> AcceptanceScope {
    match operation {
        CommandOperation::InstanceCreate => AcceptanceScope::RuntimeResource,
        CommandOperation::InstanceSend => AcceptanceScope::NativeInput,
        CommandOperation::InstanceCancel
        | CommandOperation::InstanceClose
        | CommandOperation::InteractionRespond => AcceptanceScope::NativeControl,
        _ => AcceptanceScope::RuntimeResource,
    }
}

fn settle_command(
    command: &mut Command,
    outcome: SettlementOutcome,
    error_message: Option<String>,
    execution: remuda_protocol::ExecutionState,
) -> Result<(), NodeError> {
    let now = timestamp_now()?;
    command.state = CommandState::Settled;
    command.resolution = ResolutionState::Clear;
    command.settlement = Knowledge::Known {
        value: Settlement {
            outcome,
            result_ref: None,
            error: error_message.map(|message| runtime_error(message, execution)),
        },
    };
    command.settled_at = Knowledge::Known { value: now.clone() };
    command.meta.updated_at = now;
    command.meta.revision.0 = command.meta.revision.0.saturating_add(1);
    Ok(())
}

fn runtime_error(
    message: String,
    execution: remuda_protocol::ExecutionState,
) -> remuda_protocol::RuntimeError {
    remuda_protocol::RuntimeError {
        code: remuda_protocol::ErrorCode::NativeProtocolError,
        rpc_code: remuda_protocol::ErrorCode::NativeProtocolError.rpc_code(),
        message,
        retry: remuda_protocol::RetryAction::Never,
        execution,
        details: remuda_protocol::ErrorDetails {
            command_id: None,
            instance_id: None,
            interaction_id: None,
            expected_generation: None,
            actual_generation: None,
            evidence_event_ids: None,
            native_error_ref: None,
            retry_after_ms: None,
        },
    }
}

fn settle_without_driver(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    command: &mut Command,
) -> Result<(), NodeError> {
    settle_command(
        command,
        SettlementOutcome::Completed,
        None,
        remuda_protocol::ExecutionState::NotDispatched,
    )?;
    store.save_command(command.clone())?;
    append_command_lifecycle(store, instance_id, command, "settled")
}

fn reject_before_dispatch(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    mut command: Command,
    reason: &str,
) -> Result<Command, NodeError> {
    settle_command(
        &mut command,
        SettlementOutcome::Rejected,
        Some(reason.to_owned()),
        remuda_protocol::ExecutionState::NotDispatched,
    )?;
    store.save_command(command.clone())?;
    append_command_lifecycle(store, instance_id, &command, "settled")?;
    Ok(command)
}

fn append_command_lifecycle(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    command: &Command,
    state: &str,
) -> Result<(), NodeError> {
    let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Entity(Box::new(
        EntityLifecycle {
            entity_id: command.command_id.as_id().clone(),
            revision: command.meta.revision,
            previous_state: None,
            state: state.to_owned(),
            reason_code: "local-driver-ledger".to_owned(),
            evidence_event_ids: Vec::new(),
            entity_value: LifecycleEntity::Command(Box::new(command.clone())),
        },
    ))));
    store.append_observation(instance_id, None, Completeness::Structured, payload)?;
    Ok(())
}

/// Move the instance entity to a new lifecycle and journal the transition.
///
/// The entity is updated first and the journal event appended second, so a
/// reader that observes the new lifecycle always finds the event that explains
/// it (the opposite ordering left a window the UI's poll lands in under load).
pub(crate) fn journal_instance_phase(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    lifecycle: InstanceLifecycle,
    previous_state: &str,
    reason: &str,
) -> Result<(), NodeError> {
    let state = serde_json::to_value(lifecycle)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    store.set_instance_state(instance_id, Some(lifecycle), None)?;
    append_instance_lifecycle(store, instance_id, Some(previous_state), &state, reason)
}

pub(crate) fn append_instance_lifecycle(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    previous_state: Option<&str>,
    state: &str,
    reason: &str,
) -> Result<(), NodeError> {
    let instance = store.get_instance(instance_id)?;
    let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Entity(Box::new(
        EntityLifecycle {
            entity_id: instance_id.as_id().clone(),
            revision: instance.meta.revision,
            previous_state: previous_state.map(str::to_owned),
            state: state.to_owned(),
            reason_code: reason.to_owned(),
            evidence_event_ids: Vec::new(),
            entity_value: LifecycleEntity::Instance(Box::new(instance)),
        },
    ))));
    store.append_observation(instance_id, None, Completeness::Structured, payload)?;
    Ok(())
}

fn digest_json<T: Serialize>(value: &T) -> Result<WireDigest, NodeError> {
    let bytes = serde_json::to_vec(value)?;
    let digest = Sha256::digest(bytes);
    Ok(format!("sha256:{digest:x}").try_into()?)
}

fn fixture_host(host_id: HostId) -> Result<Host, NodeError> {
    let now = timestamp_now()?;
    Ok(Host {
        meta: EntityMeta {
            id: host_id,
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now.clone(),
        },
        label: "local-dev".to_owned(),
        owner_principal_id: Id::new("prn")?,
        state: HostState::Online,
        node_version: Knowledge::Known {
            value: env!("CARGO_PKG_VERSION").to_owned(),
        },
        platform: Knowledge::Known {
            value: Platform {
                os: std::env::consts::OS.to_owned(),
                arch: std::env::consts::ARCH.to_owned(),
                path_style: PathStyle::Posix,
            },
        },
        identity_key_id: Id::new("obj")?,
        node_epoch: Knowledge::Known {
            value: Id::new("epoch")?,
        },
        transport: HostTransport {
            mode: HostTransportMode::OutboundWss,
            endpoint_ref: Id::new("obj")?,
        },
        last_seen_at: Knowledge::Known { value: now },
        lease_expires_at: unknown("local-dev-no-lease"),
        driver_inventory: crate::inventory::driver_inventory(),
        journal_id: Id::new("obj")?,
        durable_seq: U64(0),
    })
}

pub(crate) fn fixture_workspace(
    workspace_id: WorkspaceId,
    host_id: HostId,
    root: &Path,
) -> Result<Workspace, NodeError> {
    crate::workspace_access_check(root)?;
    let now = timestamp_now()?;
    let root = root.to_string_lossy().into_owned();
    Ok(Workspace {
        meta: EntityMeta {
            id: workspace_id,
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        host_id,
        label: root
            .rsplit(std::path::MAIN_SEPARATOR)
            .find(|segment| !segment.is_empty())
            .unwrap_or("workspace")
            .to_owned(),
        root_path: root.clone(),
        canonical_root: Knowledge::Known { value: root },
        state: WorkspaceState::Ready,
        repository: Knowledge::NotApplicable,
        worktree: None,
        write_policy: WritePolicy::Exclusive,
        writer_leases: Vec::new(),
        access_policy_revision: U64(1),
        journal_id: Id::new("obj")?,
        durable_seq: U64(0),
    })
}

#[cfg(test)]
pub(crate) fn fixture_instance(
    instance_id: InstanceId,
    host_id: HostId,
    workspace_id: WorkspaceId,
    driver: remuda_protocol::DriverKind,
) -> Result<Instance, NodeError> {
    fixture_instance_with_session(instance_id, host_id, workspace_id, driver, None)
}

/// Build the Instance entity, optionally seeded with the session it resumes.
///
/// Before the driver reports its own `session` lifecycle the Node has no proof
/// of the native identity, so a new Instance records the placeholder below and
/// [`crate::LocalStore::set_native_session`] corrects it. A resumed Instance
/// already knows the identity: it is the one being continued (D-026).
pub(crate) fn fixture_instance_with_session(
    instance_id: InstanceId,
    host_id: HostId,
    workspace_id: WorkspaceId,
    driver: remuda_protocol::DriverKind,
    resume_session_id: Option<&str>,
) -> Result<Instance, NodeError> {
    let now = timestamp_now()?;
    let native_session_id = resume_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(|| instance_id.as_id().to_string(), str::to_owned);
    Ok(Instance {
        meta: EntityMeta {
            id: instance_id,
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        host_id: host_id.clone(),
        workspace_id,
        kind: AgentKind::Claude,
        driver,
        lifecycle: InstanceLifecycle::Ready,
        activity: Knowledge::Known {
            value: Activity::Idle,
        },
        activity_evidence_event_ids: Vec::new(),
        connectivity: Connectivity::Connected,
        ownership: Ownership::Managed,
        native_ref: NativeRef {
            host_id,
            native_store_id: Id::new("obj")?,
            kind: AgentKind::Claude,
            session_id: Knowledge::Known {
                value: native_session_id.clone(),
            },
            transcript: unknown("fake-driver-no-transcript"),
            signal_tier: None,
            capabilities: Vec::new(),
            codex: None,
            acp: None,
            claude: Some(ClaudeRef {
                session_id: native_session_id,
            }),
            claude_bg: None,
            agy: None,
            herdr: None,
        },
        process_ref: ProcessRef {
            process_generation: U64(1),
            process_identity: unknown("fake-driver-no-process"),
            connection_epoch: Id::new("epoch")?,
        },
        spec_revision: U64(1),
        launch_id: Knowledge::Known {
            value: Id::new("launch")?,
        },
        capabilities: crate::inventory::driver_capability_snapshot(driver),
        owner_fence: U64(1),
        active_run_ids: Vec::new(),
        parent: None,
        journal_id: Id::new("obj")?,
        durable_seq: U64(0),
        exit: Knowledge::NotApplicable,
        last_error: None,
        // Created as this kind; promotion (D-025) is what changes both.
        mode: Some(remuda_protocol::InstanceMode::Native),
        promoted_at: None,
        // Remuda ran the launch command. Promotion flips this to `user`
        // (D-028 §1.0 rule 4); it records provenance, never capability.
        launched_by: Some(remuda_protocol::LaunchedBy::Remuda),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeDriver, MemoryStore};
    use std::time::Duration;

    fn native_lifecycle(
        topic: remuda_protocol::LifecycleTopic,
        name: &str,
        native_id: &str,
        related: &[(&str, &str)],
    ) -> remuda_protocol::Observation {
        let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            remuda_protocol::NativeLifecycle {
                topic,
                native_name: name.to_owned(),
                native_id: Knowledge::Known {
                    value: native_id.to_owned(),
                },
                status: Knowledge::Known {
                    value: "started".to_owned(),
                },
                related_ids: related
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
                data_ref: None,
                severity: remuda_protocol::Severity::Info,
                affects_completion: false,
            },
        ))));
        remuda_protocol::Observation {
            schema_version: remuda_protocol::SchemaVersion,
            event_id: remuda_protocol::EventId::new(),
            journal_id: Id::new("obj").expect("journal id"),
            instance_id: InstanceId::new(),
            run_id: None,
            host_id: HostId::new(),
            process_generation: U64(1),
            run_generation: None,
            seq: U64(1),
            observed_at: timestamp_now().expect("now"),
            native_at: unknown("test"),
            source: crate::driver::runtime_source(
                &fixture_instance(
                    InstanceId::new(),
                    HostId::new(),
                    WorkspaceId::new(),
                    remuda_protocol::DriverKind::ClaudePrint,
                )
                .expect("fixture instance"),
                U64(1),
            ),
            completeness: Completeness::Structured,
            raw_ref: None,
            evidence_event_ids: Vec::new(),
            body: payload,
        }
    }

    /// A live claude-print run emits several hook lifecycles after its session
    /// event, and `nativeId` there is the *hook* id. Recording one left
    /// nativeRef holding an id `claude --resume` rejects (D-026).
    #[test]
    fn only_session_bearing_lifecycles_are_read_as_the_native_session() {
        let session = native_lifecycle(
            remuda_protocol::LifecycleTopic::Session,
            "session",
            "2a99b5d4-f182-425f-ab5b-7f3541dee9bc",
            &[("model", "claude-sonnet")],
        );
        let evidence = native_session_evidence(&session).expect("session lifecycle");
        assert_eq!(evidence.session_id, "2a99b5d4-f182-425f-ab5b-7f3541dee9bc");
        assert!(evidence.transcript_path.is_none());

        let hook = native_lifecycle(
            remuda_protocol::LifecycleTopic::Hook,
            "hook_started",
            "f121381b-c2c9-47fd-a253-f5efea4e4c94",
            &[("hookEvent", "SessionStart"), ("hookId", "f121381b")],
        );
        assert!(
            native_session_evidence(&hook).is_none(),
            "a hook id is not a session id"
        );

        // claude-pty's own SessionStart hook does carry the session, and proves
        // it by naming the transcript the session writes to.
        let pty_hook = native_lifecycle(
            remuda_protocol::LifecycleTopic::Hook,
            "SessionStart",
            "3b7c1f20-0000-4000-8000-00000000aaaa",
            &[("transcriptPath", "/tmp/session.jsonl")],
        );
        let evidence = native_session_evidence(&pty_hook).expect("pty SessionStart");
        assert_eq!(evidence.session_id, "3b7c1f20-0000-4000-8000-00000000aaaa");
        assert_eq!(
            evidence.transcript_path.as_deref(),
            Some("/tmp/session.jsonl")
        );
    }

    #[tokio::test]
    async fn legacy_claude_pty_session_start_keeps_screen_activity_enabled() {
        use remuda_protocol::{DriverKind, SourceChannel};
        let store: Arc<dyn LocalStore> = Arc::new(MemoryStore::new(64));
        let instance = fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ClaudePty,
        )
        .unwrap();
        let id = instance.meta.id.clone();
        store.insert_instance(instance).unwrap();
        let interactions = InteractionRuntime::spawn(Arc::clone(&store)).unwrap();
        let (tx, rx) = mpsc::channel(8);
        let pump = spawn_observation_pump(
            Arc::clone(&store),
            interactions,
            id.clone(),
            rx,
            Arc::new(FakeDriver::default()),
            Arc::new(crate::prompt_correlation::PromptCorrelator::default()),
        );
        let mut session = native_lifecycle(
            remuda_protocol::LifecycleTopic::Hook,
            "SessionStart",
            "legacy-session",
            &[("transcriptPath", "/tmp/legacy-session.jsonl")],
        );
        session.source.driver_kind = DriverKind::ClaudePty;
        session.source.channel = SourceChannel::Hook;
        tx.send(session).await.unwrap();
        for (status, expected) in [("working", Activity::Working), ("idle", Activity::Idle)] {
            let mut screen = native_lifecycle(
                remuda_protocol::LifecycleTopic::Turn,
                "agent_status",
                "",
                &[],
            );
            screen.source.driver_kind = DriverKind::ClaudePty;
            screen.source.channel = SourceChannel::Pty;
            if let ObservationPayload::Lifecycle(payload) = &mut screen.body
                && let LifecyclePayload::Native(native) = payload.as_mut()
            {
                native.status = Knowledge::Known {
                    value: status.into(),
                };
            }
            tx.send(screen).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let current = store.get_instance(&id).unwrap();
                    if current.activity == (Knowledge::Known { value: expected }) {
                        assert_eq!(current.native_ref.signal_tier, None);
                        assert!(matches!(current.native_ref.session_id,
                            Knowledge::Known { value } if value == "legacy-session"));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("legacy screen activity remains authoritative");
        }
        drop(tx);
        pump.await.unwrap();
    }

    #[tokio::test]
    async fn promoted_signal_bus_drives_activity_only_for_the_bound_session() {
        use remuda_protocol::{DriverKind, RunId, SignalTier, SourceChannel};
        use remuda_signal::{BusContext, HookEnvelope, SignalBus};
        let store: Arc<dyn LocalStore> = Arc::new(MemoryStore::new(64));
        let instance = fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ShellPty,
        )
        .unwrap();
        let id = instance.meta.id.clone();
        store.insert_instance(instance.clone()).unwrap();
        let interactions = InteractionRuntime::spawn(Arc::clone(&store)).unwrap();
        let (tx, rx) = mpsc::channel(32);
        let bus = SignalBus::new(
            BusContext {
                // Real drivers mint a local id before the Node binds their
                // dedicated observation receiver to its own instance id.
                instance_id: InstanceId::new(),
                host_id: instance.host_id.clone(),
                journal_id: instance.journal_id.clone(),
                run_id: RunId::new(),
                driver_kind: DriverKind::ShellPty,
                adapter_version: "test".into(),
            },
            tx.clone(),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );
        let pump = spawn_observation_pump(
            Arc::clone(&store),
            interactions,
            id.clone(),
            rx,
            Arc::new(FakeDriver::default()),
            Arc::new(crate::prompt_correlation::PromptCorrelator::default()),
        );

        async fn settled(
            store: &Arc<dyn LocalStore>,
            id: &InstanceId,
            tx: &mpsc::Sender<remuda_protocol::Observation>,
        ) -> Instance {
            let marker = Id::new("obj").unwrap().to_string();
            tx.send(native_lifecycle(
                remuda_protocol::LifecycleTopic::Diagnostic,
                "pump_barrier",
                &marker,
                &[],
            ))
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let instance = store.get_instance(id).unwrap();
                    let page = store.read_events(&instance.journal_id, None, 256).unwrap();
                    if page.events.iter().any(|event| match event {
                        JournalEvent::Instance(event) => matches!(&event.body, ObservationPayload::Lifecycle(payload)
                            if matches!(payload.as_ref(), LifecyclePayload::Native(native)
                                if native.native_name == "pump_barrier" && matches!(&native.native_id, Knowledge::Known { value } if value == &marker))),
                        _ => false,
                    }) {
                        return instance;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }).await.expect("observation pump must settle within one second")
        }
        let hook = |name: &str, pid, session: &str| HookEnvelope {
            credential: "test".into(),
            event: name.into(),
            ppid: pid,
            // Native payloads cannot opt into the Node-owned annotation.
            payload: serde_json::json!({"session_id": session, "transcript_path": "/tmp/hook-session.jsonl", "remudaActivity":"working"}),
        };
        bus.handle(hook("SessionStart", 42, "session-a")).await;
        bus.handle(hook("UserPromptSubmit", 42, "session-a")).await;
        let first = settled(&store, &id, &tx).await;
        assert_eq!(
            first.native_ref.signal_tier, None,
            "SessionStart alone cannot claim another foreground"
        );
        let mut promoted = native_lifecycle(
            remuda_protocol::LifecycleTopic::Session,
            "agent_promoted",
            "",
            &[("kind", "claude"), ("pid", "42")],
        );
        promoted.source.driver_kind = DriverKind::ShellPty;
        promoted.source.channel = SourceChannel::Runtime;
        tx.send(promoted).await.unwrap();
        let bound = settled(&store, &id, &tx).await;
        assert_eq!(bound.native_ref.signal_tier, Some(SignalTier::Hook));
        assert_eq!(
            bound.activity,
            Knowledge::Known {
                value: Activity::Working
            },
            "a prompt accepted before the promotion poll must not be lost"
        );
        assert!(
            matches!(bound.native_ref.session_id, Knowledge::Known { value } if value == "session-a")
        );
        let deferred = store
            .read_events(&instance.journal_id, Some(first.durable_seq), 16)
            .unwrap();
        assert!(deferred.events.iter().any(|event| matches!(event,
            JournalEvent::Instance(event) if matches!(&event.body,
                ObservationPayload::Lifecycle(payload) if matches!(payload.as_ref(),
                    LifecyclePayload::Native(native) if native.native_name == "agent_promoted"
                        && native.related_ids.get("remudaActivity").map(String::as_str) == Some("working"))))));
        let before_foreign = bound.durable_seq;
        for (pid, session) in [(99, "session-a"), (42, "foreign-session")] {
            let mut display = hook("MessageDisplay", pid, session);
            // Reusing the following owned message id proves a rejected final
            // chunk cannot close or otherwise poison its assembler state.
            display.payload["message_id"] = serde_json::json!("streamed-message");
            display.payload["index"] = serde_json::json!(9);
            display.payload["delta"] = serde_json::json!("foreign text");
            display.payload["final"] = serde_json::json!(true);
            bus.handle(display).await;
        }
        let before_stream = settled(&store, &id, &tx).await.durable_seq;
        let foreign_events = store
            .read_events(&instance.journal_id, Some(before_foreign), 16)
            .unwrap();
        assert_eq!(
            foreign_events
                .events
                .iter()
                .filter(|event| matches!(event,
            JournalEvent::Instance(event) if matches!(&event.body,
                ObservationPayload::Lifecycle(payload) if matches!(payload.as_ref(),
                    LifecyclePayload::Native(native) if native.native_name == "MessageDisplay"))))
                .count(),
            2,
            "foreign hooks remain raw journal evidence"
        );
        assert!(!foreign_events.events.iter().any(|event| matches!(event,
            JournalEvent::Instance(event) if matches!(&event.body, ObservationPayload::Message(_)))),
            "foreign hooks cannot create foreground assistant messages");

        // The same pump must retain P3 streaming while binding hook activity.
        for (index, text, final_chunk) in [(0, "first ", false), (1, "second", true)] {
            let mut display = hook("MessageDisplay", 42, "session-a");
            display.payload["message_id"] = serde_json::json!("streamed-message");
            display.payload["index"] = serde_json::json!(index);
            display.payload["delta"] = serde_json::json!(text);
            display.payload["final"] = serde_json::json!(final_chunk);
            bus.handle(display).await;
        }
        let streamed = settled(&store, &id, &tx).await;
        assert_eq!(
            streamed.activity,
            Knowledge::Known {
                value: Activity::Working
            }
        );
        assert_eq!(streamed.native_ref.signal_tier, Some(SignalTier::Hook));
        let stream_events = store
            .read_events(&instance.journal_id, Some(before_stream), 16)
            .unwrap();
        let mut displays = Vec::new();
        let mut messages = Vec::new();
        for event in &stream_events.events {
            let JournalEvent::Instance(event) = event else {
                continue;
            };
            match &event.body {
                ObservationPayload::Lifecycle(payload)
                    if matches!(payload.as_ref(),
                    LifecyclePayload::Native(native) if native.native_name == "MessageDisplay") =>
                {
                    displays.push(event.seq)
                }
                ObservationPayload::Message(payload) => messages.push((event.seq, payload)),
                _ => {}
            }
        }
        assert_eq!(displays.len(), 2);
        assert_eq!(messages.len(), 2, "streaming remains in the merged pump");
        assert!(
            messages
                .iter()
                .zip(&displays)
                .all(|((seq, _), hook_seq)| seq > hook_seq)
        );
        assert_eq!(
            messages[0].1.mutation.node_id,
            messages[1].1.mutation.node_id
        );
        assert_eq!(
            messages[0].1.mutation.operation,
            remuda_protocol::MutationOperation::Open
        );
        assert_eq!(
            messages[1].1.mutation.operation,
            remuda_protocol::MutationOperation::Append
        );
        assert_eq!(messages[1].1.mutation.base_revision, Some(U64(1)));
        for ((_, message), (text, status)) in messages.iter().zip([
            ("first ", remuda_protocol::ContentStatus::Streaming),
            ("second", remuda_protocol::ContentStatus::Complete),
        ]) {
            assert_eq!(message.status, status);
            assert!(
                matches!(&message.blocks[0], remuda_protocol::ContentBlock::Text(block) if block.text == text)
            );
        }
        bus.handle(hook("Stop", 42, "session-a")).await;
        settled(&store, &id, &tx).await;

        for (name, pid, session, expected) in [
            ("UserPromptSubmit", 99, "session-a", Activity::Idle),
            ("UserPromptSubmit", 42, "foreign-session", Activity::Idle),
            ("UserPromptSubmit", 42, "session-a", Activity::Working),
            ("Stop", 99, "session-a", Activity::Working),
            ("SubagentStop", 42, "session-a", Activity::Working),
            ("Stop", 42, "session-a", Activity::Idle),
            ("SubagentStop", 42, "session-a", Activity::Idle),
            ("UserPromptSubmit", 42, "session-a", Activity::Working),
            ("StopFailure", 42, "session-a", Activity::Idle),
            (
                "PermissionRequest",
                42,
                "session-a",
                Activity::WaitingInteraction,
            ),
            ("Stop", 42, "session-a", Activity::Idle),
        ] {
            let before = store.get_instance(&id).unwrap().durable_seq;
            bus.handle(hook(name, pid, session)).await;
            let current = settled(&store, &id, &tx).await;
            assert_eq!(
                current.activity,
                Knowledge::Known { value: expected },
                "{name} from {pid}/{session}"
            );
            let events = store
                .read_events(&instance.journal_id, Some(before), 16)
                .unwrap();
            let raw = events
                .events
                .iter()
                .find_map(|event| match event {
                    JournalEvent::Instance(event) => match &event.body {
                        ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
                            LifecyclePayload::Native(native) if native.native_name == name => {
                                Some((event.seq, native))
                            }
                            _ => None,
                        },
                        _ => None,
                    },
                    _ => None,
                })
                .expect("causal hook observation");
            let marker = if pid == 42 && session == "session-a" {
                match name {
                    "UserPromptSubmit" => Some("working"),
                    "Stop" | "StopFailure" => Some("idle"),
                    _ => None,
                }
            } else {
                None
            };
            assert_eq!(
                raw.1.related_ids.get("remudaActivity").map(String::as_str),
                marker,
                "only owned turn evidence may carry the annotation"
            );
            let mirrored = events.events.iter().find_map(|event| match event {
                JournalEvent::Instance(event) if event.seq > raw.0 => match &event.body {
                    ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
                        LifecyclePayload::Entity(entity)
                            if entity.reason_code == "hook-activity" =>
                        {
                            match &entity.entity_value {
                                LifecycleEntity::Instance(instance) => Some(&instance.activity),
                                _ => None,
                            }
                        }
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            });
            if marker.is_some() {
                assert!(
                    mirrored.is_none(),
                    "annotated turn evidence is not duplicated"
                );
            } else if expected == Activity::WaitingInteraction {
                assert_eq!(
                    mirrored,
                    Some(&Knowledge::Known {
                        value: Activity::WaitingInteraction
                    }),
                    "waiting-interaction retains its full-entity propagation"
                );
            }
        }
        // Lower-tier screen evidence must not overwrite the harness boundary.
        let mut screen = native_lifecycle(
            remuda_protocol::LifecycleTopic::Session,
            "agent_status",
            "",
            &[],
        );
        screen.source.driver_kind = DriverKind::ShellPty;
        screen.source.channel = SourceChannel::Pty;
        if let ObservationPayload::Lifecycle(payload) = &mut screen.body
            && let LifecyclePayload::Native(native) = payload.as_mut()
        {
            native.status = Knowledge::Known {
                value: "working".into(),
            };
        }
        tx.send(screen).await.unwrap();
        assert_eq!(
            settled(&store, &id, &tx).await.activity,
            Knowledge::Known {
                value: Activity::Idle
            }
        );

        let mut demoted = native_lifecycle(
            remuda_protocol::LifecycleTopic::Session,
            "agent_demoted",
            "",
            &[("kind", "terminal")],
        );
        demoted.source.driver_kind = DriverKind::ShellPty;
        demoted.source.channel = SourceChannel::Runtime;
        tx.send(demoted).await.unwrap();
        assert_eq!(settled(&store, &id, &tx).await.native_ref.signal_tier, None);
        bus.handle(hook("UserPromptSubmit", 42, "session-a")).await;
        assert_eq!(
            settled(&store, &id, &tx).await.activity,
            Knowledge::Known {
                value: Activity::Idle
            }
        );
        drop(bus);
        drop(tx);
        pump.await.unwrap();
    }

    #[tokio::test]
    async fn driver_panic_is_contained_and_marks_dispatch_unknown() {
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_roots(remuda_testing::test_workspace_roots!());
        let store = Arc::new(MemoryStore::new(8));
        let drivers = DriverRegistry::default();
        drivers
            .register(Arc::new(
                FakeDriver::default().with_panic_prompt("panic-fixture"),
            ))
            .expect("register fake driver");
        let node = DevNode::with_parts(&config, store, drivers).expect("compose node");
        let request: CreateInstanceRequest = serde_json::from_value(serde_json::json!({
            "kind": "claude",
            "driver": "claude-print",
            "prompt": "panic-fixture"
        }))
        .expect("create request");
        let created = node
            .create_instance(request)
            .await
            .expect("accepted create");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let instance = node
                    .get_instance(&created.instance.meta.id)
                    .expect("instance remains visible");
                if instance.lifecycle == InstanceLifecycle::Failed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panic monitor completed");
        let command = node
            .get_command(&created.command.command_id)
            .expect("command remains visible");
        assert_eq!(command.resolution, ResolutionState::Unknown);
        let instance = node
            .get_instance(&created.instance.meta.id)
            .expect("failed instance");
        assert!(
            instance.last_error.is_some(),
            "failed instance must set lastError"
        );
        let events = node
            .read_journal(&instance.journal_id, None, 128)
            .expect("task-exit journal");
        assert!(events.events.iter().any(|event| {
            let JournalEvent::Instance(observation) = event else {
                return false;
            };
            let ObservationPayload::Lifecycle(payload) = &observation.body else {
                return false;
            };
            matches!(
                payload.as_ref(),
                LifecyclePayload::Entity(entity) if entity.state == "failed"
            )
        }));
        node.shutdown()
            .await
            .expect("already-ended worker is settled");
        let exited = node.get_instance(&instance.meta.id).unwrap();
        assert_eq!(exited.lifecycle, InstanceLifecycle::Exited);
        assert!(matches!(exited.exit, Knowledge::Known { .. }));
    }

    /// D-028 P2 follow-up: the RPC reply proves only the durable accept. A
    /// driver whose materialization sleeps (slow `pin_binary`, overlay/shim
    /// generation, PTY spawn) must not delay the create: the response lands
    /// immediately with an accepted command and a `preparing` instance, and
    /// the worker walks the instance to `ready` afterwards.
    #[tokio::test]
    async fn create_is_accepted_before_a_slow_launch_finishes() {
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_roots(remuda_testing::test_workspace_roots!());
        let store = Arc::new(MemoryStore::new(8));
        let drivers = DriverRegistry::default();
        // The shared instance: every built fake starts 400 ms after spawn.
        let slow = Arc::new(
            crate::FakeDriver::new(remuda_protocol::DriverKind::ClaudePrint)
                .with_start_delay(Duration::from_millis(400)),
        );
        drivers.register(slow).expect("register slow fake");
        let node = DevNode::with_parts(&config, store, drivers).expect("node");

        let started = std::time::Instant::now();
        let created = node
            .create_instance(
                serde_json::from_value(serde_json::json!({
                    "kind": "claude",
                    "driver": "claude-print",
                    "prompt": "",
                }))
                .expect("request"),
            )
            .await
            .expect("create");
        // The accept does not wait for the 400 ms launch.
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "durable ack took {:?}; it must not wait for materialization",
            started.elapsed()
        );
        assert_eq!(
            created.command.state,
            CommandState::Accepted,
            "the create command is accepted in the reply"
        );
        assert_eq!(
            created.instance.lifecycle,
            InstanceLifecycle::Preparing,
            "the instance is accepted but not yet materialized"
        );

        // The worker then materializes and journals starting → ready.
        let final_instance = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let instance = node
                    .get_instance(&created.instance.meta.id)
                    .expect("instance");
                if instance.lifecycle == InstanceLifecycle::Ready {
                    return instance;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reaches ready");
        assert_eq!(final_instance.lifecycle, InstanceLifecycle::Ready);

        // Preparing/starting transitions are visible in the journal, which is
        // how a Hub that never got the RPC reply converges anyway.
        let states: Vec<String> = node
            .read_journal(&created.instance.journal_id, None, 128)
            .expect("journal")
            .events
            .iter()
            .filter_map(|event| {
                let JournalEvent::Instance(observation) = event else {
                    return None;
                };
                let ObservationPayload::Lifecycle(payload) = &observation.body else {
                    return None;
                };
                match payload.as_ref() {
                    LifecyclePayload::Entity(entity)
                        if matches!(entity.entity_value, LifecycleEntity::Instance(_)) =>
                    {
                        Some(entity.state.clone())
                    }
                    _ => None,
                }
            })
            .collect();
        assert!(states.contains(&"preparing".to_owned()), "{states:?}");
        assert!(states.contains(&"starting".to_owned()), "{states:?}");
        assert!(states.contains(&"ready".to_owned()), "{states:?}");
        node.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn tty_write_dispatches_send_keys_on_fake_driver() {
        let node = DevNode::new(
            &crate::DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        )
        .expect("node");
        let created = node
            .create_instance(
                serde_json::from_value(serde_json::json!({
                    "kind": "claude",
                    "driver": "claude-print",
                    "prompt": ""
                }))
                .expect("request"),
            )
            .await
            .expect("create");
        let result = crate::transport::hubnode::dispatch_method(
            &node,
            remuda_protocol::hubnode::METHOD_TTY_WRITE,
            serde_json::json!({
                "instanceId": created.instance.meta.id,
                "keys": ["enter"]
            }),
        )
        .await
        .expect("tty.write");
        assert!(result.get("error").is_none(), "{result}");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let instance = node
                    .get_instance(&created.instance.meta.id)
                    .expect("instance");
                let page = node
                    .read_journal(&instance.journal_id, None, 64)
                    .expect("journal");
                if page.events.iter().any(|event| {
                    let JournalEvent::Instance(observation) = event else {
                        return false;
                    };
                    let ObservationPayload::Lifecycle(payload) = &observation.body else {
                        return false;
                    };
                    let LifecyclePayload::Native(native) = payload.as_ref() else {
                        return false;
                    };
                    native.native_name == "fake-driver-keys"
                        && native.status
                            == Knowledge::Known {
                                value: "enter".into(),
                            }
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake-driver-keys lifecycle");
    }

    #[tokio::test]
    async fn instance_configure_is_forwarded_and_applied_on_fake_claude() {
        let node = DevNode::new(
            &crate::DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        )
        .expect("node");
        let created = node
            .create_instance(
                serde_json::from_value(serde_json::json!({
                    "kind": "claude",
                    "driver": "claude-print",
                    "prompt": ""
                }))
                .expect("request"),
            )
            .await
            .expect("create");
        let result = crate::transport::hubnode::dispatch_method(
            &node,
            remuda_protocol::hubnode::METHOD_INSTANCE_CONFIGURE,
            serde_json::json!({
                "instanceId": created.instance.meta.id,
                "origin": "human",
                "model": "opus",
                "effort": { "index": 3, "name": "ultracode", "kind": "claude" }
            }),
        )
        .await
        .expect("instance.configure");
        assert!(result.get("error").is_none(), "{result}");
        let command_id = result["command"]["commandId"]
            .as_str()
            .expect("commandId")
            .to_owned();
        let command_id = CommandId::try_from(command_id).expect("command id");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let command = node.get_command(&command_id).expect("command");
                if command.operation == CommandOperation::InstanceConfigure
                    && command.state == CommandState::Settled
                {
                    assert_eq!(command.origin, CommandOrigin::Ui);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("configure command settled");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let instance = node
                    .get_instance(&created.instance.meta.id)
                    .expect("instance");
                let page = node
                    .read_journal(&instance.journal_id, None, 64)
                    .expect("journal");
                let has_command = page.events.iter().any(|event| {
                    let JournalEvent::Instance(observation) = event else {
                        return false;
                    };
                    let ObservationPayload::Lifecycle(payload) = &observation.body else {
                        return false;
                    };
                    let LifecyclePayload::Entity(entity) = payload.as_ref() else {
                        return false;
                    };
                    matches!(
                        entity.entity_value,
                        LifecycleEntity::Command(ref command)
                            if command.operation == CommandOperation::InstanceConfigure
                    )
                });
                let has_apply = page.events.iter().any(|event| {
                    let JournalEvent::Instance(observation) = event else {
                        return false;
                    };
                    let ObservationPayload::Lifecycle(payload) = &observation.body else {
                        return false;
                    };
                    let LifecyclePayload::Native(native) = payload.as_ref() else {
                        return false;
                    };
                    native.native_name == "instance.configure"
                        && native.status
                            == Knowledge::Known {
                                value: "applied model=opus effort=ultracode index=3".into(),
                            }
                });
                if has_command && has_apply {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("configure command entity and driver apply");
    }

    #[test]
    fn tty_key_allowlist_accepts_cli_keys_and_rejects_parser_sequences() {
        for key in [
            "enter",
            "RETURN",
            "tab",
            "esc",
            "escape",
            "space",
            "backspace",
            "bs",
            "delete",
            "del",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "c-c",
            "ctrl+a",
            "CTRL+Z",
            "a",
            "A",
            "中",
            ";",
        ] {
            assert!(validate_tty_key(key).is_ok(), "{key:?}");
        }
        for key in [
            "",
            " ",
            "\n",
            "\0",
            "\x1b",
            "\u{7f}",
            "\x1b[31m",
            "enter; whoami",
            "text:payload",
            " ctrl+c",
            "enter\n",
            "ctrl+1",
            "ctrl+aa",
            "ctrl+é",
            "alt+enter",
            "unknown-key",
        ] {
            assert!(validate_tty_key(key).is_err(), "{key:?}");
        }
    }

    #[tokio::test]
    async fn tty_write_rpc_rejects_entire_invalid_batch_before_dispatch() {
        let node = DevNode::new(
            &crate::DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        )
        .expect("node");
        let created = node
            .create_instance(
                serde_json::from_value(serde_json::json!({
                    "kind": "claude", "driver": "claude-print", "prompt": ""
                }))
                .expect("request"),
            )
            .await
            .expect("create");
        for keys in [
            serde_json::json!([]),
            serde_json::json!(["enter", "text:payload"]),
            serde_json::json!(["\u{001b}[31m"]),
        ] {
            let rejected = crate::transport::hubnode::dispatch_method(
                &node,
                remuda_protocol::hubnode::METHOD_TTY_WRITE,
                serde_json::json!({"instanceId": created.instance.meta.id, "keys": keys}),
            )
            .await;
            assert!(
                rejected.is_err(),
                "invalid logical keys must not reach the driver"
            );
        }
        let events = node
            .read_journal(&created.instance.journal_id, None, 128)
            .expect("journal");
        assert!(
            !serde_json::to_string(&events)
                .expect("events")
                .contains("fake-driver-keys")
        );
    }
}
