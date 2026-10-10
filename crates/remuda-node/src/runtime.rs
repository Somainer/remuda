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
    pub(crate) interactions: Arc<InteractionRuntime>,
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
    /// Uploader for read-only host-file fetches (D-031-era host files slice).
    /// `None` until the outbound link gives us a Hub HTTP origin and token.
    pub(crate) host_file_stager: std::sync::RwLock<Option<Arc<dyn crate::files::HostFileStager>>>,
    /// Shared slot holding the stager for image blocks in tool results
    /// (D-045 §6.2). The same `Arc` is handed to the native driver factories
    /// at compose time; the outbound link fills it on every hello. Writes go
    /// through [`DevNode::set_tool_media_stager`].
    pub(crate) tool_media_stager: std::sync::RwLock<crate::ToolMediaStagerSlot>,
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
    /// D-047/D-048 model-API relay: per-instance loopback listeners, the
    /// proxy-side egress contexts, and the live carrier link's frame broker.
    pub(crate) api_relay: Arc<crate::api_relay::ApiRelayState>,
    diagnostics: std::sync::RwLock<crate::DoctorContext>,
}

/// In-process Node used by the development REST/JSON-RPC/WS surface.
#[derive(Clone)]
pub struct DevNode {
    pub(crate) inner: Arc<DevNodeInner>,
}

/// Last-clone teardown.
///
/// A Node dropped without [`crate::service::RunningNode::shutdown`] (the
/// restart tests do exactly this to simulate a hard process death) used to leave
/// its instance workers, observation pumps, and interaction tasks running: they
/// held store clones, so the journal writer thread stayed alive with its
/// SQLite/JSONL handles, and a Node reopened on the same data dir raced a
/// "dropped" writer allocating the same seq and writing at a stale JSONL offset
/// — `UNIQUE constraint failed: events.instance_id, events.seq` and
/// `json: EOF while parsing a string` in the landing gate.
///
/// Everything here is synchronous and best-effort: reject new commands, abort
/// the store-owning tasks, then close the journal explicitly so the
/// single-writer lock is released even while aborted tasks still drain their
/// store clones. Writers that run one more instruction after abort can no
/// longer reach the journal.
impl Drop for DevNodeInner {
    fn drop(&mut self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.interactions.shutdown();
        abort_task_map(&self.workers);
        abort_task_map(&self.pumps);
        self.store.shutdown_journal();
    }
}

/// Abort every handle in an instance-keyed task map without awaiting.
///
/// Drop-safe `try_lock` only. The maps are held across short inserts and are
/// drained first by the async shutdown path; contended teardown means a spawn
/// in flight, whose journal calls now fail because the journal is closed.
fn abort_task_map(map: &tokio::sync::Mutex<BTreeMap<InstanceId, tokio::task::JoinHandle<()>>>) {
    if let Ok(mut guard) = map.try_lock() {
        for (_, handle) in std::mem::take(&mut *guard) {
            handle.abort();
        }
    } else {
        tracing::warn!("node teardown skipped task abort: task map briefly locked");
    }
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
                host_file_stager: std::sync::RwLock::new(None),
                tool_media_stager: std::sync::RwLock::new(Arc::new(std::sync::RwLock::new(None))),
                attachment_root: std::sync::RwLock::new(None),
                queue_capacity: config.instance_queue_capacity,
                host,
                workspace,
                workspace_registry: std::sync::RwLock::new(workspace_registry),
                projection_epoch: Id::new("epoch")?,
                tty: TtyRegistry::new(),
                gate: crate::gate::GateRegistry::new(),
                api_relay: crate::api_relay::ApiRelayState::new(),
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

    /// D-047/D-048 relay state: listeners, proxy egress contexts, and the live
    /// link's `api.*` broker.
    #[must_use]
    pub(crate) fn api_relay(&self) -> &std::sync::Arc<crate::api_relay::ApiRelayState> {
        &self.inner.api_relay
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

    /// The instance inventory to announce at hello, or `None` when it cannot
    /// be vouched for.
    ///
    /// The Hub settles every live row a hello omits, so an *empty* inventory
    /// is a destructive claim: it says this Node holds nothing, and the Hub
    /// acts on that by exiting every session on the host. A Node that cannot
    /// attest that it actually found its instance store — a `--data-dir`
    /// pointed somewhere else, a wiped disk — enumerates zero rows without
    /// that meaning anything, so it reports `None` and the hello carries no
    /// `instances` key at all. The Hub reads an absent key as "cannot
    /// compare" and leaves the rows alone, which is the only safe reading.
    pub fn announceable_inventory(&self) -> Result<Option<Value>, NodeError> {
        let items = self.inner.store.list_instances()?;
        if items.is_empty() && !self.inner.store.instance_store_is_durable() {
            tracing::warn!(
                "instance store not found under this data dir; refusing to announce an \
                 empty inventory (it would settle every row on this host)"
            );
            return Ok(None);
        }
        serde_json::to_value(items).map(Some).map_err(Into::into)
    }

    /// Whether this Node found a durable instance store where it expected one.
    ///
    /// The attestation that travels with the inventory. It is what lets a Hub
    /// honour an empty inventory — "I really do hold nothing" — instead of
    /// having to treat every empty list as a possible wrong `--data-dir`.
    #[must_use]
    pub fn found_instance_store(&self) -> bool {
        self.inner.store.instance_store_is_durable()
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
                        driver.kind(),
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
        // D-047: purge is definitive — revoke the bearer and shut the listener
        // even when the worker task already ended without running its terminal
        // block (abort, crash double, stale rows).
        self.inner
            .api_relay
            .revoke_instance(instance_id.as_id().as_str());
        let mut directory_removed = false;
        if let Some(config) = &self.inner.herdr_config {
            let dir = config
                .data_dir
                .join("instances")
                .join(instance_id.as_id().as_str());
            // Under a long data dir the real hook socket lives in the
            // per-user runtime dir and `hook.sock` here is only a symlink;
            // remove_dir_all would unlink the link but leak the inode.
            // Best-effort: log but do not fail purge if the target is gone.
            if let Err(error) =
                remuda_signal::runtime_dir::unlink_resolved_socket_link(&dir.join("hook.sock"))
            {
                tracing::debug!(
                    instance = %instance_id.as_id(),
                    %error,
                    "could not unlink the redirected hook socket while purging"
                );
            }
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
        // The purged instance may have held the last reference to a pinned
        // hook relay version; collect any now-unreferenced ones.
        self.collect_hook_relays();
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

    /// `worktree_rpc` with the blocking git work off the async runtime's
    /// threads, under the carrier's in-flight cap.
    ///
    /// `handle_rpc` shells out to git (`worktree create` clones and fetches), so
    /// awaiting it on a runtime worker — which is all a spawn from the select
    /// loop would achieve — parks that worker for the duration. On a small host
    /// with one or two workers, enough of these park the carrier loop itself,
    /// `gate.cancel` included.
    pub(crate) async fn worktree_rpc_capped(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Value, NodeError> {
        let selected = params
            .get("workspaceId")
            .and_then(Value::as_str)
            .map(str::parse)
            .transpose()?;
        let (workspace, _) = self.resolve_workspace_cwd(selected.as_ref(), None)?;
        let method = method.to_owned();
        let params = params.clone();
        crate::gate::run_long_method(move || {
            crate::worktree::handle_rpc(Path::new(&workspace.root_path), &method, &params)
                .ok_or_else(|| NodeError::InvalidRequest(format!("unknown method {method}")))?
        })
        .await
    }

    /// Resolve the command a raced loser must echo after losing the
    /// `insert_instance` race against a *live* existing instance:
    ///
    /// * the winner's row for this command id already accepted → echo it;
    /// * the winner's row exists but is still Queued → bounded wait for it to
    ///   reach Accepted/Settled, erroring on the deadline rather than
    ///   returning a queued row the Hub would reject;
    /// * no row exists for this command id (the loser carried a fresh id) →
    ///   durably accept the loser's own command against the existing instance
    ///   and journal it, without launching anything.
    async fn command_for_raced_create(
        &self,
        instance_id: &InstanceId,
        mut command: Command,
    ) -> Result<Command, NodeError> {
        match self.inner.store.get_command(&command.command_id) {
            Ok(recorded)
                if matches!(
                    recorded.state,
                    CommandState::Accepted | CommandState::Settled
                ) =>
            {
                Ok(recorded)
            }
            Ok(_) => {
                self.wait_for_accepted_create_command(&command.command_id)
                    .await
            }
            Err(_) => {
                accept_command(&mut command)?;
                if self
                    .inner
                    .store
                    .insert_command(instance_id, command.clone())?
                {
                    append_command_lifecycle(
                        self.inner.store.as_ref(),
                        instance_id,
                        &command,
                        "accepted",
                    )?;
                }
                Ok(command)
            }
        }
    }

    /// Wait briefly for a racing winner's create command to reach the ledger
    /// in an accepted state. A create that lost the `insert_instance` race
    /// must echo the winner's *accepted* command (the Hub rejects queued
    /// replies), so the loser yields while the winner moves from insert
    /// (queued) to accept + save. The window is bounded: a winner that stalls
    /// mid-accept surfaces an error instead of a queued reply.
    async fn wait_for_accepted_create_command(
        &self,
        command_id: &CommandId,
    ) -> Result<Command, NodeError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(command) = self.inner.store.get_command(command_id)
                && matches!(
                    command.state,
                    CommandState::Accepted | CommandState::Settled
                )
            {
                return Ok(command);
            }
            if std::time::Instant::now() >= deadline {
                return Err(NodeError::Conflict(format!(
                    "command {} did not reach an accepted state in time",
                    command_id.as_id()
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
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
        // Refuse a driver this Node has no adapter for, here, before the command
        // is durably accepted — never downgrade to one it does have. Building
        // runs later off the accept path, so an unregistered driver would
        // otherwise be answered "accepted" and then die in the worker, leaving
        // the Hub with a running row for a product that never started
        // (docs/design/evidence/dispatch-driver-1.md).
        if !self.inner.drivers.is_registered(request.driver) {
            return Err(NodeError::InvalidRequest(format!(
                "unsupported-driver: no {:?} adapter is registered on this node",
                request.driver
            )));
        }
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

        // Command ids default to the empty string in the dev runtime; allocate
        // the rest of the idempotency bookkeeping once, up front.
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

        // Idempotency, instance-id form: a retried create may carry a fresh
        // command id but the same client-allocated instance id. If that
        // instance already exists and has not terminated, this is the same
        // intent — echo its recorded projection and first-bound route without
        // launching a second worker or binding a second listener.
        if let Ok(existing_instance) = self.inner.store.get_instance(&instance_id)
            && !matches!(
                existing_instance.lifecycle,
                InstanceLifecycle::Exited | InstanceLifecycle::Failed
            )
        {
            let command = match self.inner.store.get_command(&command_id) {
                // Same-command-id retry: echo the ledger's accepted command.
                Ok(recorded) => recorded,
                // A retry that reused only the instance id carries a fresh
                // command id. It must not echo the *queued* command built
                // above (the Hub accepts only accepted/settled rows and would
                // log "node did not durably accept"): accept this command id
                // durably first, journal the row, then reply with it. The
                // instance and worker are the first attempt's; no second
                // bind or launch happens.
                Err(_) => {
                    set_command_origin(&mut command, request.origin);
                    accept_command(&mut command)?;
                    // insert_command is the create path that links the
                    // command to the instance's ledger; save_command only
                    // replaces an existing row. A racing identical insert
                    // answers with the recorded command instead.
                    if self
                        .inner
                        .store
                        .insert_command(&instance_id, command.clone())?
                    {
                        append_command_lifecycle(
                            self.inner.store.as_ref(),
                            &instance_id,
                            &command,
                            "accepted",
                        )?;
                        command
                    } else {
                        self.inner.store.get_command(&command_id)?
                    }
                }
            };
            return Ok(CreateInstanceResponse {
                command,
                instance: existing_instance,
                api_route: self
                    .inner
                    .api_relay
                    .observed_route(instance_id.as_id().as_str()),
            });
        }

        set_command_origin(&mut command, request.origin);
        // Idempotency pre-check before the instance row exists: an exact
        // command-id + payload match means an earlier attempt accepted it, so
        // return its recorded projection without binding anything new.
        let duplicate = {
            let existing = self.inner.store.get_command(&command_id).ok();
            matches!(
                existing,
                Some(existing)
                    if existing.payload_digest == command.payload_digest
                        && existing.operation == command.operation
            )
        };
        if duplicate {
            return Ok(CreateInstanceResponse {
                command: self.inner.store.get_command(&command_id)?,
                instance: self.inner.store.get_instance(&instance_id)?,
                // Echo the route the first accepted attempt recorded, never
                // None and never a re-decision (D-035).
                api_route: self
                    .inner
                    .api_relay
                    .observed_route(instance_id.as_id().as_str()),
            });
        }
        // New command: now bind the relay. Provisioning is idempotent per
        // instance id, so even here an existing listener is reused rather than
        // evicted. A bind/probe failure fails this command with no launch —
        // never a silent fallback to direct (D-035).
        let mut provisioned = crate::api_relay::listener::provision_for_request(
            &self.inner.api_relay,
            instance_id.as_id().as_str(),
            &request,
        )
        .await?;
        let observed_route = provisioned
            .as_ref()
            .map(|provisioned| provisioned.observed.clone());
        let mut provision_guard = provisioned
            .as_mut()
            .and_then(|provisioned| provisioned.guard.take());
        let relay_overlay = provisioned
            .as_ref()
            .map(|provisioned| provisioned.overlay.clone());
        let launch = DriverLaunch {
            instance: instance.clone(),
            request: request.clone(),
            workspace_root,
            registered_workspace_root: workspace.root_path.into(),
            api_relay: relay_overlay,
        };
        // The instance insert is the idempotency point for two creates whose
        // provisions overlapped the multi-second route probe (the per-instance
        // provision lock keeps the listener bind single-writer, but the loser
        // reaches this insert while the winner is accepting). The Conflict
        // fallback is scoped to a raced create of a *live* instance: a
        // terminated (Exited/Failed) row keeps the original error, and the
        // provision guard below is dropped on that error so the freshly bound
        // listener is revoked.
        if let Err(insert_error) = self.inner.store.insert_instance(instance) {
            if !matches!(insert_error, NodeError::Conflict(_)) {
                return Err(insert_error);
            }
            let existing_instance = self.inner.store.get_instance(&instance_id)?;
            if matches!(
                existing_instance.lifecycle,
                InstanceLifecycle::Exited | InstanceLifecycle::Failed
            ) {
                return Err(NodeError::Conflict(format!(
                    "instance {} already exists",
                    instance_id.as_id()
                )));
            }
            let command = self.command_for_raced_create(&instance_id, command).await?;
            // Fallback succeeded: this listener is the winner's (provision is
            // idempotent per instance id), so commit rather than revoke. Any
            // error return above drops the guard and revokes the bind.
            if let Some(guard) = provision_guard.take() {
                guard.commit();
            }
            return Ok(CreateInstanceResponse {
                command,
                instance: existing_instance,
                api_route: self
                    .inner
                    .api_relay
                    .observed_route(instance_id.as_id().as_str())
                    .or(observed_route),
            });
        }
        // Defensive: with the provision lock and the instance-insert catch
        // above, a false here can only come from another writer; answer
        // idempotently rather than failing the create.
        let inserted = self
            .inner
            .store
            .insert_command(&instance_id, command.clone())?;
        if !inserted {
            if let Some(guard) = provision_guard.take() {
                guard.commit();
            }
            return Ok(CreateInstanceResponse {
                command: self.inner.store.get_command(&command_id)?,
                instance: self.inner.store.get_instance(&instance_id)?,
                api_route: self
                    .inner
                    .api_relay
                    .observed_route(instance_id.as_id().as_str())
                    .or(observed_route),
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
        // The worker now owns the listener for the instance's lifetime and
        // revokes it from its terminal block; failure to read the row back is
        // the one path that revokes here.
        let instance = match self.inner.store.get_instance(&instance_id) {
            Ok(instance) => instance,
            Err(error) => {
                self.inner
                    .api_relay
                    .revoke_instance(instance_id.as_id().as_str());
                return Err(error);
            }
        };
        if let Some(guard) = provision_guard.take() {
            guard.commit();
        }
        Ok(CreateInstanceResponse {
            command,
            instance,
            api_route: observed_route,
        })
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

    /// Pin the hook relay at Node start so its content-addressed copy captures
    /// the build the Node is running, not whatever replaces it before the first
    /// hooked launch. Best-effort: a failure is logged and retried per launch.
    /// A fake-driver Node (no `herdr_config`) has no relay to pin.
    pub(crate) fn pin_hook_relay(&self) {
        let Some(config) = &self.inner.herdr_config else {
            return;
        };
        crate::hook_shim::for_data_dir(&config.data_dir).pin_now();
    }

    /// Collect `hook-bin/<version>` directories no live instance references.
    ///
    /// Called at Node start and after `instance.purge`. The version the running
    /// Node pinned is always kept. A fake-driver Node (no `herdr_config`) has no
    /// pinned relay and nothing to collect.
    pub(crate) fn collect_hook_relays(&self) {
        let Some(config) = &self.inner.herdr_config else {
            return;
        };
        let running = crate::hook_shim::for_data_dir(&config.data_dir).pinned_if_ready();
        crate::hook_shim::garbage_collect(&config.data_dir, running.as_deref());
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

    /// Local store backing this Node (internal RPC helpers).
    pub(crate) fn store(&self) -> &dyn crate::LocalStore {
        self.inner.store.as_ref()
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

    /// JSONL payload bytes this Node has read from journals since start.
    ///
    /// Instrumentation for the tail-only flush: one flush tick over a caught-up
    /// Instance should add roughly the size of the newly appended events, not
    /// the size of the journal.
    #[must_use]
    pub fn journal_bytes_read(&self) -> u64 {
        self.inner.store.journal_bytes_read()
    }

    /// Durable sequence the journal's own store has committed.
    pub fn journal_durable_seq(&self, journal_id: &Id) -> Result<U64, NodeError> {
        self.inner.store.journal_durable_seq(journal_id)
    }

    /// Durable sequence of an Instance's journal, resolved through its identity.
    ///
    /// Flush ticks use this to prove they are caught up without opening the
    /// reader at all.
    pub fn current_journal_durable_seq(&self, instance_id: &InstanceId) -> Result<U64, NodeError> {
        let journal_id = self.get_instance(instance_id)?.journal_id;
        self.journal_durable_seq(&journal_id)
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
        let api_relay = Arc::clone(&self.inner.api_relay);
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
                    api_relay.revoke_instance(worker_instance.as_id().as_str());
                    if let Some(node) = node.upgrade() {
                        node.senders.write().await.remove(&worker_instance);
                    }
                    return;
                }
                Err(join_error) => {
                    let reason = format!("driver build task panicked: {join_error}");
                    tracing::error!(%reason, "instance driver build panicked");
                    record_task_exit(store.as_ref(), &worker_instance, &reason);
                    api_relay.revoke_instance(worker_instance.as_id().as_str());
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
            // The instance has run to completion (or died): revoke its relay
            // bearer and shut the loopback listener. Idempotent with the
            // explicit close/purge and shutdown paths below.
            api_relay.revoke_instance(worker_instance.as_id().as_str());
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

/// The explicit pin a launch gate arms from: the recipe's `model_pin`
/// (`spec.model_id`, no profile fallback), trimmed and empties dropped.
///
/// Deliberately not `model_requested`, which is also set to the profile's
/// default model on an unpinned launch; arming from it would make an unpinned
/// launch refuseable.
fn pin_from_recipe(recipe: &Option<remuda_driver::LaunchRecipe>) -> Option<String> {
    normalize_explicit_pin(recipe.as_ref()?.provider.model_pin.as_deref())
}

/// Trim a pin and treat a blank/absent one as "no explicit pin".
fn normalize_explicit_pin(pin: Option<&str>) -> Option<String> {
    pin.map(str::trim)
        .filter(|pin| !pin.is_empty())
        .map(str::to_owned)
}

/// Post-launch model-pin read-back (`evidence/model-pin-1.md` §3, §5).
///
/// model-pin-1: a `--model` pin is sent, but a host layer can still make the
/// session answer on something else. This gate reads back what actually
/// answered using the alias-aware comparison in
/// [`remuda_protocol::compare_model_pin`] so a correct gateway launch
/// (`acme_hub/model_x_o50[1m]` answered by `claude-opus-5`) is not flagged.
///
/// It is deliberately event-driven and fail-open: it judges only the
/// **launch-attributed** read-back, once. If no genuine read-back ever arrives
/// (no assistant message, no verdict), there is no evidence of a divergence, so
/// nothing is recorded. A later in-session switch — a human `/model` or a
/// Remuda `configure` — is out of scope by source attribution.
///
/// Owner ruling 2026-09-23 (`model-pin-1.md` §5): a divergence is **recorded,
/// never acted on**. The harness provides the capability; the agent decides
/// what it runs. The gate must not close the process or fail the instance — it
/// returns one [`ModelDivergence`] the pump journals as a warning diagnostic
/// naming both ids verbatim, and the session keeps running.
struct ModelPinGate {
    /// The requested id. `None` disables the gate entirely: a pin that was
    /// never requested cannot mismatch.
    pin: Option<String>,
    /// The session's discovered model list, from the launch snapshot. Lets an
    /// un-namespaced observation be recognised as catalog vocabulary.
    catalog: Vec<String>,
    /// The launch read-back has been judged; report at most once.
    settled: bool,
}

/// A launch read-back that named a model other than the pin, in the pin's own
/// vocabulary. Recorded honestly and then ignored: the session is not stopped.
#[derive(Debug, PartialEq, Eq)]
struct ModelDivergence {
    /// The requested pin, verbatim.
    pin: String,
    /// The id that actually answered, verbatim.
    observed: String,
}

impl ModelPinGate {
    fn new(pin: Option<String>) -> Self {
        Self {
            pin,
            catalog: Vec::new(),
            settled: false,
        }
    }

    /// Fold one observation. `Some` means the launch read-back proved a
    /// divergence: the caller records it and keeps the session running.
    fn observe(&mut self, observation: &remuda_protocol::Observation) -> Option<ModelDivergence> {
        let pin = self.pin.as_deref()?;
        let ObservationPayload::Model(payload) = &observation.body else {
            return None;
        };
        // The launch snapshot carries the discovered list; keep it regardless
        // of whether the observation itself is judged.
        if let Some(catalog) = payload.catalog.as_ref() {
            self.catalog = catalog.models.clone();
        }
        if self.settled {
            return None;
        }
        // Only the launch read-back is in scope. The driver emits two
        // launch-sourced model observations:
        //
        // * the *snapshot* (`take_launch_model_snapshot`): emitted before the
        //   process speaks, its `effective.id` is the requested pin itself and
        //   it has no `raw` transcript spelling. It is a prediction, not a
        //   read-back — judging it would settle the gate on our own request and
        //   make the check unfalsifiable;
        // * the first genuine read-back: an assistant record's `message.model`
        //   or a launch-settled `/model` verdict. It carries the native spelling
        //   in `raw`, so `raw.is_some()` is what separates it from the
        //   snapshot. Its `source` is `Launch` because the launch seeded the
        //   attribution; later edges (`Unknown`) and human/Remuda switches
        //   (`Slash`/`Remuda`) are not this gate's business.
        let is_launch_readback = payload.effective.source == remuda_protocol::EffortSource::Launch
            && payload.raw.is_some();
        if !is_launch_readback {
            return None;
        }
        self.settled = true;
        let observed = payload.effective.id.trim();
        match remuda_protocol::compare_model_pin(pin, observed, &self.catalog) {
            remuda_protocol::ModelPinVerdict::Mismatch => Some(ModelDivergence {
                pin: pin.to_owned(),
                observed: observed.to_owned(),
            }),
            // Honoured (the pin, or its context-suffix spelling) and
            // Unresolvable (a gateway resolved it to an upstream vendor name —
            // indistinguishable from a correct launch on this channel) both
            // pass silently.
            remuda_protocol::ModelPinVerdict::Honoured
            | remuda_protocol::ModelPinVerdict::Unresolvable => None,
        }
    }
}

/// Stable machine reason on the diagnostic that records a launch model pin
/// that was not honoured (`model-pin-1.md` §5). Recorded, never fatal: the
/// session keeps running and the instance is not failed.
const MODEL_MISMATCH: &str = "model-mismatch";

/// The journal record for a launch read-back that named a different model in
/// the pin's own vocabulary.
///
/// Reuses the native diagnostic shape — no new wire type or field — and carries
/// both ids verbatim in `related_ids`. It is a warning that does not affect
/// completion, because the owner's 2026-09-23 ruling is that the harness
/// records what answered; it does not stop the agent from running on it.
fn model_pin_diagnostic(pin: &str, observed: &str) -> ObservationPayload {
    ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
        remuda_protocol::NativeLifecycle {
            topic: remuda_protocol::LifecycleTopic::Diagnostic,
            native_name: "model_pin_mismatch".to_owned(),
            native_id: Knowledge::NotApplicable,
            status: Knowledge::Known {
                value: "diverged".to_owned(),
            },
            related_ids: [
                ("reason".to_owned(), MODEL_MISMATCH.to_owned()),
                ("requested".to_owned(), pin.to_owned()),
                ("observed".to_owned(), observed.to_owned()),
            ]
            .into_iter()
            .collect(),
            data_ref: None,
            severity: remuda_protocol::Severity::Warning,
            affects_completion: false,
        },
    ))))
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
        // model-pin-1 §5: record what the launch actually read back, then keep
        // running. The gate arms from `model_pin` — `spec.model_id` only,
        // never the profile-derived default that also populates
        // `model_requested` on an unpinned launch — so a launch with no pin
        // records nothing. Owner ruling 2026-09-23: a divergence is not the
        // harness's decision to act on; it is a fact to journal.
        let mut model_pin = ModelPinGate::new(pin_from_recipe(&driver.launch_recipe()));
        loop {
            tokio::select! {
                maybe_observation = observations.recv() => {
                    let Some(observation) = maybe_observation else { break };
                    // Fold the observation first, so the model edge that proves
                    // the divergence is in the journal before the diagnostic
                    // that explains it.
                    let divergence = model_pin.observe(&observation);
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
                    // Record requested vs observed verbatim and let the session
                    // run: no Close, no task exit, no failed instance.
                    if let Some(divergence) = divergence {
                        tracing::warn!(
                            pin = %divergence.pin,
                            observed = %divergence.observed,
                            "launch model pin not honoured; recording and continuing"
                        );
                        if let Err(error) = store.append_observation(
                            &instance_id,
                            None,
                            Completeness::Structured,
                            model_pin_diagnostic(&divergence.pin, &divergence.observed),
                        ) {
                            tracing::error!(%error, "model pin diagnostic commit failed");
                        }
                    }
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
    // a finished instance reported as `ready`. A subagent's lifecycle (the
    // failed workflow member in the bug report) and any non-session/non-
    // completion observation is never the main process exiting.
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
    //
    // Finally, the print/sdk engine's own turn events (ma-sdk-state r3):
    // turn/turn_started sets working; a settled turn/result idles exactly
    // like a successful turn end even when the result is a turn failure
    // (c-cardsettle r5 item 4) — never failing the instance. The
    // cardsettle r5-item-4 fallback (an UNSETTLED root result/error still
    // frees the composer) must run AFTER the explicit engine decision: the
    // merge with ma-sdk-state had dropped it, which let a mapper-bypassed
    // failed result stick the root at working (the Hub derive keeps both
    // arms; Node and Hub must agree).
    let activity = hook_activity
        .or_else(|| match store.get_instance(instance_id) {
            Ok(instance) if crate::adapter_registry::has_file_adapter(instance.kind) => {
                crate::signal::file_activity(&observation)
            }
            _ => None,
        })
        .or_else(|| crate::signal::engine_turn_activity(&observation))
        .or_else(|| root_turn_failure_activity(&observation));
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
            // c-cardsettle r7 item 7 (OA6): the Node's OWN store failing to
            // commit an observation is not evidence the child ended. End the
            // instance only when the driver reports the child actually gone;
            // otherwise keep it (and its cards) alive and append a best-effort
            // error observation so the failure is visible once the store
            // recovers. The pump keeps running, so the driver's later real
            // exit still settles the instance normally.
            if driver.process_gone().await {
                tracing::error!(%error, instance_id = %instance_id.as_id(), "native observation commit failed with the child gone");
                record_task_exit(
                    store.as_ref(),
                    instance_id,
                    "native-observation-commit-failed",
                );
            } else {
                tracing::error!(%error, instance_id = %instance_id.as_id(), "native observation commit failed; child alive, keeping the instance live");
                let diagnostic = DriverEmission::NativeLifecycle {
                    name: "observation-commit-failed".to_owned(),
                    status: format!("rejected: {error}"),
                    severity: remuda_protocol::Severity::Warning,
                };
                match diagnostic.into_payload() {
                    Ok(payload) => {
                        if let Err(diag_error) = store.append_observation(
                            instance_id,
                            None,
                            Completeness::Structured,
                            payload,
                        ) {
                            tracing::warn!(%diag_error, "commit-failure diagnostic also not journaled; instance stays live");
                        }
                    }
                    Err(payload_error) => {
                        tracing::warn!(%payload_error, "commit-failure diagnostic could not be built; instance stays live");
                    }
                }
            }
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
                    mode: remuda_protocol::PromptMode::NewTurn,
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
                // A locally drained answer (terminal/local API) has no Hub
                // frame; stamp human so the committed actor stays the
                // pre-D-051 Human shape rather than defaulting to Agent.
                serde_json::json!({
                    "interactionId": interaction_id,
                    "answer": answer,
                    "commandId": command.command_id.as_id().as_str(),
                    "origin": "human",
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
            finish_instance_operation(store.as_ref(), instance_id, driver.kind(), &queued.request)?;
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
                // c-cardsettle r7 item 7 (OA6): a rejected command is not a
                // process end. End the instance only when the driver itself
                // reports the child gone (e.g. stdin closed against an exited
                // process); with the child alive the diagnostic appended above
                // is the whole record, the instance keeps running and its
                // pending cards stay pending — the driver's own real exit
                // observation settles them through the normal pump path.
                if driver.process_gone().await {
                    record_task_exit(store.as_ref(), instance_id, &error.to_string());
                } else {
                    tracing::warn!(%error, instance_id = %instance_id.as_id(), "command rejected while the child is alive; instance stays live");
                }
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
    kind: remuda_protocol::DriverKind,
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
            // ma-sdk-state r4 item 2: on the structured print/sdk carriers a
            // transport acknowledgement is not turn evidence. The
            // turn_started observation sets working and the SETTLED result (or
            // process end) idles; stamping idle here overwrote a working that
            // the pump applied from turn_started, and any successful control
            // during a turn did the same. PTY carriers have no native turn
            // lifecycle, so the ack stays their idle signal.
            if !crate::signal::structured_engine_turn_carrier(kind) {
                store.set_instance_state(
                    instance_id,
                    None,
                    Some(Knowledge::Known {
                        value: Activity::Idle,
                    }),
                )?;
            }
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
    // c-cardsettle r5 addendum (OA6): use the SHARED process-end classifier.
    // Only a real Failed process end returns a reason; Exited is not a failure;
    // everything else (configure/turn/severity=error on a live process) returns
    // None so record_task_exit never appends entity state=failed.
    use remuda_protocol::process_end::{ProcessEndKind, process_end_observation};
    // Subagent scope is never the main process, regardless of the event.
    if is_subagent_observation(native) {
        return None;
    }
    // The full-observation variant carries the evidence timestamp (`at`) for
    // ended_at stamping; only Failed ends the task with entity state=failed.
    let end = process_end_observation(observation)?;
    if end.kind != ProcessEndKind::Failed {
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

/// c-cardsettle r3 item 8 / r4 item 3: a native lifecycle attributed to a
/// SUBAGENT (a non-empty `relatedIds.agentId`) is the subagent's row, never
/// the main instance. `agentType` is OPTIONAL — some producers stamp only the
/// id. Main-session observations carry no agentId.
pub(crate) fn is_subagent_observation(native: &remuda_protocol::NativeLifecycle) -> bool {
    native
        .related_ids
        .get("agentId")
        .is_some_and(|id| !id.is_empty())
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
    // r3 item 8: a subagent's exit observation is the subagent's row, never
    // the main process.
    if is_subagent_observation(native) {
        return None;
    }
    // c-cardsettle r5 addendum (OA6): use the SHARED process-end classifier.
    // Accept BOTH the shell-pty (native_exit) and print/SDK (session) real
    // exit events, classified Exited vs Failed by the classifier.
    use remuda_protocol::process_end::{ProcessEndKind, process_end_observation};
    let end = process_end_observation(observation)?;
    let Knowledge::Known { value: _status } = &native.status else {
        return None;
    };
    // Map the classifier outcome to the status string record_native_exit uses.
    let state = match end.kind {
        ProcessEndKind::Exited => "exited",
        ProcessEndKind::Failed => "failed",
    };
    Some(NativeExit {
        state: state.to_string(),
        reason: native
            .related_ids
            .get("reason")
            .cloned()
            .unwrap_or_else(|| state.to_string()),
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
    // c-cardsettle r5 addendum: a Failed exit stays Failed; a clean exit is
    // Exited. Do not unconditionally overwrite Failed with Exited (equal-rank
    // overwrite turned non-zero exits / signals into clean exits).
    let end_lifecycle = if exit.state == "failed" {
        InstanceLifecycle::Failed
    } else {
        InstanceLifecycle::Exited
    };
    if let Err(error) = store.set_instance_state(
        instance_id,
        Some(end_lifecycle),
        Some(Knowledge::Known {
            value: Activity::Idle,
        }),
    ) {
        tracing::error!(%error, "instance not marked after its process ended");
    }
}

/// c-cardsettle r5 item 4 (OA6): the activity edge for a ROOT turn that ended
/// FAILED. The print/SDK mapper emits `topic=turn`, `nativeName=result`,
/// `status=error` (claude_print `map_result`); the shell hook fold emits a
/// root `StopFailure` with `outcome=failed`. Both free the composer (idle) so
/// the human can retry in place; the PROCESS stays alive — only
/// `process_end` evidence is terminal. Subagent scope, configure and
/// diagnostic topics return None (own scope). Mirrors the Hub's
/// `root_turn_failed` derivation so Node local state and the Hub agree.
///
/// Runs in the observation pump AFTER
/// `signal::engine_turn_activity`: that fold handles turn_started and the
/// explicit `settledRootTurn` decision, while this is the r5-item-4 fallback
/// for an UNSETTLED result/error (older journals and mapper-bypassed tests).
/// In the live mapper output a root error already carries the settled flag,
/// so the two arms agree on the real stream.
fn root_turn_failure_activity(observation: &remuda_protocol::Observation) -> Option<Activity> {
    let ObservationPayload::Lifecycle(payload) = &observation.body else {
        return None;
    };
    let LifecyclePayload::Native(native) = payload.as_ref() else {
        return None;
    };
    if is_subagent_observation(native) || native.topic != remuda_protocol::LifecycleTopic::Turn {
        return None;
    }
    // Shell-pty HOOK events are attributed only when the promoted-hook
    // tracker verifies ownership (that fold already returns Idle for a bound
    // root StopFailure). An unbound hook could belong to a different
    // foreground session, so this channel-agnostic fallback must NOT invent
    // attribution for it. Print/SDK children own the Stdout/Transcript
    // channels outright — those are exactly the events this exists for.
    if observation.source.channel == remuda_protocol::SourceChannel::Hook
        && observation.source.driver_kind == remuda_protocol::DriverKind::ShellPty
    {
        return None;
    }
    let outcome_failed = native
        .related_ids
        .get("outcome")
        .is_some_and(|outcome| outcome.eq_ignore_ascii_case("failed"));
    // ma-sdk-state r4 item 2: an UNSETTLED result error frees the composer
    // only for the one-shot print shape that explicitly claims
    // affects_completion (the final, unqueued result). A queued/intermediate
    // (open-workflow) result carries no claim and keeps the turn working;
    // settled results are handled by `engine_turn_activity` before this
    // fallback runs.
    let result_error = native.native_name == "result"
        && matches!(&native.status, remuda_protocol::Knowledge::Known { value } if value == "error")
        && native.affects_completion;
    let stop_failure = native.native_name == "StopFailure" && outcome_failed;
    (result_error || stop_failure).then_some(Activity::Idle)
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
                    // c-steer: a steer carries through to the PTY queue; an
                    // ordinary send and an unmarked older client are new turns.
                    mode: request
                        .prompt_mode
                        .unwrap_or(remuda_protocol::PromptMode::NewTurn),
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
                permission_mode: request.permission_mode.clone(),
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
            // print-replacement.md §2.6: a second structured carrier for kind
            // `claude` — stream-json over stdio with no `-p`, so stdin stays
            // open across turns. Explicit only; the omitted-driver order in
            // D-035 is unchanged and never resolves here.
            | (AgentKind::Claude, remuda_protocol::DriverKind::ClaudeSdk)
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
        // D-047: no relay bind configured here, so this fixture host serves
        // every `via` session over `hub-relay` and opens no extra listener.
        relay_bind: None,
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
        native_lifecycle_full(
            topic,
            name,
            native_id,
            related,
            remuda_protocol::Severity::Info,
            false,
            "started",
        )
    }

    /// c-cardsettle r3 item 1: configurable builder for the failure-classifier
    /// tests (severity / affectsCompletion / status vary).
    fn native_lifecycle_full(
        topic: remuda_protocol::LifecycleTopic,
        name: &str,
        native_id: &str,
        related: &[(&str, &str)],
        severity: remuda_protocol::Severity,
        affects_completion: bool,
        status: &str,
    ) -> remuda_protocol::Observation {
        let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            remuda_protocol::NativeLifecycle {
                topic,
                native_name: name.to_owned(),
                native_id: Knowledge::Known {
                    value: native_id.to_owned(),
                },
                status: Knowledge::Known {
                    value: status.to_owned(),
                },
                related_ids: related
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
                data_ref: None,
                severity,
                affects_completion,
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

    /// c-cardsettle r6 item 1: the drivers' STARTUP frames reuse the exit name
    /// (`topic=session, nativeName=session`) with live statuses — print/SDK
    /// `map_init` status "started", the PTY ready frames carrying the herdr
    /// agent status (idle/working/blocked). Fed through the REAL pump they
    /// must leave a live instance Ready; only the driver's later real exit
    /// ends it (print `emit_exit("exited")` → Exited).
    #[tokio::test]
    async fn startup_session_frames_never_end_the_live_instance() {
        use remuda_protocol::{DriverKind, SourceChannel};
        let store: Arc<dyn LocalStore> = Arc::new(MemoryStore::new(64));
        let instance = fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ClaudePrint,
        )
        .unwrap();
        let id = instance.meta.id.clone();
        store.insert_instance(instance).unwrap();
        let interactions = InteractionRuntime::spawn(Arc::clone(&store)).unwrap();
        let (tx, rx) = mpsc::channel(16);
        let pump = spawn_observation_pump(
            Arc::clone(&store),
            interactions,
            id.clone(),
            rx,
            Arc::new(FakeDriver::default()),
            Arc::new(crate::prompt_correlation::PromptCorrelator::default()),
        );

        let frame = |channel: SourceChannel,
                     driver: DriverKind,
                     name: &str,
                     native_id: &str,
                     status: &str,
                     severity: remuda_protocol::Severity| {
            let mut observation = native_lifecycle_full(
                remuda_protocol::LifecycleTopic::Session,
                name,
                native_id,
                &[],
                severity,
                false,
                status,
            );
            observation.source.channel = channel;
            observation.source.driver_kind = driver;
            observation
        };

        // Wait until the pump DURABLY commits an observation matching a
        // native id/status: a sleep could race the pump and assert before (or
        // long after) the frames landed. r7 item 6 replaces the 120 ms sleep
        // with this committed-seq readiness marker.
        let wait_committed =
            |native_id: &'static str,
             status: &'static str|
             -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
                let store = Arc::clone(&store) as Arc<dyn LocalStore>;
                let id = id.clone();
                let status = status.to_string();
                Box::pin(async move {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            let committed = store.get_instance(&id).is_ok_and(|instance| {
                                store
                                    .read_events(&instance.journal_id, None, 256)
                                    .is_ok_and(|page| {
                                        page.events.iter().any(|event| match event {
                                            JournalEvent::Instance(observation) => {
                                                matches!(&observation.body,
                                                    ObservationPayload::Lifecycle(payload)
                                                        if matches!(payload.as_ref(),
                                                            LifecyclePayload::Native(native)
                                                                if matches!(&native.native_id,
                                                                    Knowledge::Known { value }
                                                                        if value == native_id)
                                                                    && matches!(&native.status,
                                                                        Knowledge::Known { value }
                                                                            if value == &status)))
                                            }
                                            _ => false,
                                        })
                                    })
                            });
                            if committed {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                    })
                    .await
                    .expect("the observation pump commits the frame in time");
                })
            };

        // 1) The print/SDK init frame (stdout channel).
        tx.send(frame(
            SourceChannel::Stdout,
            DriverKind::ClaudePrint,
            "session",
            "sess-1",
            "started",
            remuda_protocol::Severity::Info,
        ))
        .await
        .unwrap();
        wait_committed("sess-1", "started").await;
        // The init frame alone leaves the instance live.
        let after_init = store.get_instance(&id).unwrap();
        assert!(
            !matches!(
                after_init.lifecycle,
                InstanceLifecycle::Failed | InstanceLifecycle::Exited
            ),
            "the print/SDK init frame never ends the session: {:?}",
            after_init.lifecycle
        );
        // 2) The Claude PTY ready frame with each live agent status.
        for status in ["idle", "working", "blocked", "done", "unknown"] {
            tx.send(frame(
                SourceChannel::Herdr,
                DriverKind::ClaudePty,
                "session",
                "pane-7",
                status,
                remuda_protocol::Severity::Info,
            ))
            .await
            .unwrap();
        }
        // 3) Even an ERROR-severity live status must not be an end.
        tx.send(frame(
            SourceChannel::Herdr,
            DriverKind::GenericPty,
            "session",
            "pane-8",
            "working",
            remuda_protocol::Severity::Error,
        ))
        .await
        .unwrap();
        wait_committed("pane-8", "working").await;

        // Lifecycle AND activity asserted only once every startup frame is
        // durable: still live, with a concrete known activity the folds
        // produced — never failed/unknown.
        let live = store.get_instance(&id).unwrap();
        assert!(
            !matches!(
                live.lifecycle,
                InstanceLifecycle::Failed | InstanceLifecycle::Exited
            ),
            "startup frames must never end the session: {:?}",
            live.lifecycle
        );
        assert!(
            matches!(live.activity, Knowledge::Known { .. }),
            "startup frames leave a concrete live activity: {:?}",
            live.activity
        );

        // 3b) A failed first TURN (the print mapper's exact result-error
        // frame on stdout): composer idles but the process stays alive — it
        // must accept the next prompt rather than being marked ended.
        tx.send(native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "result",
            "sess-1",
            &[("resultIndex", "1"), ("numTurns", "1")],
            remuda_protocol::Severity::Error,
            true,
            "error",
        ))
        .await
        .unwrap();
        // The failed result commits BEFORE the lifecycle assertion, and ends
        // the TURN (activity idle) without touching the process lifecycle.
        wait_committed("sess-1", "error").await;
        let after_turn = store.get_instance(&id).unwrap();
        assert!(
            !matches!(
                after_turn.lifecycle,
                InstanceLifecycle::Failed | InstanceLifecycle::Exited
            ),
            "a failed result is turn-level, not a process end: {:?}",
            after_turn.lifecycle
        );
        assert_eq!(
            after_turn.activity,
            Knowledge::Known {
                value: Activity::Idle
            },
            "a failed root turn frees the composer (idle)"
        );

        // 4) The REAL print driver exit: name=session, status=exited.
        tx.send(frame(
            SourceChannel::Stdout,
            DriverKind::ClaudePrint,
            "session",
            "sess-1",
            "exited",
            remuda_protocol::Severity::Info,
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.get_instance(&id).unwrap().lifecycle == InstanceLifecycle::Exited {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the real print session/exited ends the instance");

        drop(tx);
        pump.await.unwrap();
    }

    /// c-cardsettle r3 item 1: a severity=error configure observation on a live
    /// PTY (model/effort/permission switch failed) is NOT classified as a task
    /// exit — otherwise the Node journals a FAILED entity and the Hub kills the
    /// session's pending card while the process is alive.
    #[test]
    fn configure_error_is_not_a_native_failure() {
        for status in [
            "model-control-unavailable:write failed",
            "effort-control-unavailable:submit failed",
            "permission-control-unavailable:cycle failed",
        ] {
            let obs = native_lifecycle_full(
                remuda_protocol::LifecycleTopic::Configuration,
                "instance.configure",
                "not-applicable",
                &[],
                remuda_protocol::Severity::Error,
                false,
                status,
            );
            assert_eq!(
                native_failure_reason(&obs),
                None,
                "a live configure failure ({status}) must not record a task exit"
            );
            assert!(
                native_exit(&obs).is_none(),
                "a configure observation is never a native exit"
            );
        }
    }

    /// c-cardsettle r3 item 1: an explicit `affectsCompletion=false` error on
    /// any other native topic is likewise non-terminal.
    #[test]
    fn non_completion_error_is_not_a_native_failure() {
        let obs = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Session,
            "some_transient_error",
            "not-applicable",
            &[],
            remuda_protocol::Severity::Error,
            false,
            "transient",
        );
        assert_eq!(native_failure_reason(&obs), None);
    }

    /// c-cardsettle r3 item 1: a REAL process death (topic=session,
    /// affectsCompletion=true, severity=error) is still classified as a
    /// failure, so genuine exits keep ending the instance.
    #[test]
    fn real_process_failure_is_still_a_native_failure() {
        let obs = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Session,
            "exit",
            "not-applicable",
            &[("lastError", "pane exited; agent process is gone")],
            remuda_protocol::Severity::Error,
            true,
            "pane exited; agent process is gone",
        );
        assert_eq!(
            native_failure_reason(&obs).as_deref(),
            Some("pane exited; agent process is gone"),
            "a real pane exit must still end the instance"
        );
    }

    /// c-cardsettle r3 addendum: a StopFailure on the MAIN session is a TURN
    /// failure (topic=turn, affectsCompletion=false), never a process exit —
    /// `record_task_exit` must not fire, so after a failed stop the composer is
    /// usable again in the same live process.
    #[test]
    fn main_stop_failure_ends_only_the_turn_not_the_process() {
        let obs = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "StopFailure",
            "not-applicable",
            &[("outcome", "failed"), ("phase", "turn-ended")],
            remuda_protocol::Severity::Warning,
            false,
            "idle",
        );
        assert_eq!(
            native_failure_reason(&obs),
            None,
            "a main StopFailure is turn-level, not a task exit"
        );
        assert!(native_exit(&obs).is_none());
    }

    /// c-cardsettle r5 item 4: the channel-agnostic root turn-failure edge
    /// frees the composer for print/SDK Runtime/Stdout channels, but does NOT
    /// attribute an UNBOUND shell-pty hook (the promoted-hook fold verifies
    /// ownership itself); subagent/configure stay own-scope.
    #[test]
    fn root_turn_failure_activity_is_scoped_like_the_hub_projection() {
        use remuda_protocol::{DriverKind, SourceChannel};
        // Root result error on the print/SDK (Runtime) channel → idle.
        let runtime_result = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "result",
            "sess-1",
            &[("resultIndex", "1"), ("numTurns", "1")],
            remuda_protocol::Severity::Error,
            true,
            "error",
        );
        assert_eq!(
            root_turn_failure_activity(&runtime_result),
            Some(Activity::Idle)
        );

        // Root StopFailure outcome=failed on the Runtime channel → idle.
        let root_stop = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "StopFailure",
            "not-applicable",
            &[("outcome", "failed"), ("phase", "turn-ended")],
            remuda_protocol::Severity::Warning,
            false,
            "idle",
        );
        assert_eq!(root_turn_failure_activity(&root_stop), Some(Activity::Idle));

        // The SAME root result error arriving on a shell-pty HOOK is not
        // attributed by this fallback: an unbound hook may belong to another
        // foreground session (the PromotedHooks fold supplies the edge only
        // once ownership is verified).
        let mut hook_result = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "result",
            "sess-1",
            &[("resultIndex", "1")],
            remuda_protocol::Severity::Error,
            true,
            "error",
        );
        hook_result.source.channel = SourceChannel::Hook;
        hook_result.source.driver_kind = DriverKind::ShellPty;
        assert_eq!(root_turn_failure_activity(&hook_result), None);

        // A SUBAGENT result error is own-scope even on the Runtime channel.
        let sub_result = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "result",
            "not-applicable",
            &[("agentId", "a1"), ("resultIndex", "2")],
            remuda_protocol::Severity::Error,
            false,
            "error",
        );
        assert_eq!(root_turn_failure_activity(&sub_result), None);

        // A successful result (status turn_done) changes nothing.
        let turn_done = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "result",
            "sess-1",
            &[("resultIndex", "1"), ("numTurns", "1")],
            remuda_protocol::Severity::Info,
            false,
            "turn_done",
        );
        assert_eq!(root_turn_failure_activity(&turn_done), None);
    }

    /// c-cardsettle r3 item 8: any hook/turn/session observation carrying a
    /// subagent agentId is the subagent's row — even a topic=session
    /// severity=error "error" name must not fail the MAIN instance.
    #[test]
    fn subagent_observations_never_fail_the_main_instance() {
        for (topic, name, severity, completion) in [
            (
                remuda_protocol::LifecycleTopic::Turn,
                "StopFailure",
                remuda_protocol::Severity::Warning,
                false,
            ),
            (
                remuda_protocol::LifecycleTopic::Diagnostic,
                "SubagentStop",
                remuda_protocol::Severity::Info,
                false,
            ),
            // A subagent process genuinely dying is still the subagent's row.
            (
                remuda_protocol::LifecycleTopic::Session,
                "error",
                remuda_protocol::Severity::Error,
                true,
            ),
        ] {
            let obs = native_lifecycle_full(
                topic,
                name,
                "not-applicable",
                &[
                    ("agentId", "agent0sub0agent000"),
                    ("agentType", "workflow-subagent"),
                ],
                severity,
                completion,
                "idle",
            );
            assert_eq!(
                native_failure_reason(&obs),
                None,
                "subagent {name} must not fail the main instance"
            );
            assert!(
                native_exit(&obs).is_none(),
                "a subagent exit must not exit the main instance"
            );
        }
    }

    /// c-cardsettle r3 addendum — table-driven process-end vs turn-level.
    #[test]
    fn failure_signal_classification_table() {
        use remuda_protocol::Severity;
        // (label, topic, name, severity, affects_completion, related, is_terminal)
        use remuda_protocol::LifecycleTopic;
        #[allow(clippy::type_complexity)]
        let cases: Vec<(
            &str,
            LifecycleTopic,
            &str,
            Severity,
            bool,
            &[(&str, &str)],
            bool,
        )> = vec![
            // Turn-level: configure / stop / hooks / diagnostics / subagents.
            (
                "configure",
                LifecycleTopic::Configuration,
                "instance.configure",
                Severity::Error,
                false,
                &[],
                false,
            ),
            (
                "main-stop-failure",
                LifecycleTopic::Turn,
                "StopFailure",
                Severity::Warning,
                false,
                &[("outcome", "failed")],
                false,
            ),
            (
                "main-stop",
                LifecycleTopic::Turn,
                "Stop",
                Severity::Info,
                false,
                &[],
                false,
            ),
            (
                "hook-error",
                LifecycleTopic::Hook,
                "hook_failed",
                Severity::Error,
                false,
                &[],
                false,
            ),
            (
                "diagnostic-error",
                LifecycleTopic::Diagnostic,
                "api_error",
                Severity::Error,
                false,
                &[],
                false,
            ),
            (
                "subagent-stop-failure",
                LifecycleTopic::Turn,
                "StopFailure",
                Severity::Warning,
                false,
                &[
                    ("agentId", "a1"),
                    ("agentType", "workflow-subagent"),
                    ("outcome", "failed"),
                ],
                false,
            ),
            (
                "subagent-exit",
                LifecycleTopic::Session,
                "error",
                Severity::Error,
                true,
                &[("agentId", "a1"), ("agentType", "workflow-subagent")],
                false,
            ),
            // r4 item 3: agentId ALONE (agentType missing) is still subagent scope.
            (
                "subagent-id-only",
                LifecycleTopic::Session,
                "exit",
                Severity::Error,
                true,
                &[("agentId", "a1")],
                false,
            ),
            // r4 item 2: a transient topic=session error with a DISTINCT
            // name (not the exact "error" launch-failure name) is not
            // terminal even with severity=error.
            (
                "transient-session-error",
                LifecycleTopic::Session,
                "transient_runtime_error",
                Severity::Error,
                false,
                &[],
                false,
            ),
            // Terminal: only real process ends of the MAIN session.
            (
                "pane-exit",
                LifecycleTopic::Session,
                "exit",
                Severity::Error,
                true,
                &[("lastError", "pane exited")],
                true,
            ),
            (
                "startup-error",
                LifecycleTopic::Session,
                "error",
                Severity::Error,
                true,
                &[
                    ("lastError", "agent process exited during startup"),
                    ("reasonCode", "native-driver-start-failed"),
                ],
                true,
            ),
            (
                "gone",
                LifecycleTopic::Session,
                "agent_gone",
                Severity::Error,
                true,
                &[],
                true,
            ),
        ];
        for (label, topic, name, severity, completion, related, terminal) in cases {
            let obs = native_lifecycle_full(
                topic,
                name,
                "not-applicable",
                related,
                severity,
                completion,
                "idle",
            );
            let is_failure = native_failure_reason(&obs).is_some();
            assert_eq!(
                is_failure, terminal,
                "{label}: expected terminal={terminal}, got native_failure_reason={is_failure}"
            );
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

    /// c-cardsettle r3 item 1 — the REAL producer sequence. When a model/
    /// effort/permission switch fails on a live PTY, the driver journals an
    /// `instance.configure` lifecycle (topic=configuration, severity=error,
    /// affectsCompletion=false). The Node's severity classifier must NOT call
    /// record_task_exit for it, so a pending approval on the live session is
    /// not killed — the instance stays Ready/Running. A subsequent REAL process
    /// death (topic=session, affectsCompletion=true) still fails it.
    #[tokio::test]
    async fn configure_error_pump_keeps_the_live_instance_running_then_real_exit_fails() {
        use remuda_protocol::{Activity, DriverKind, Knowledge};
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

        // Exactly what claude_pty::journal emits for a failed live switch
        // (model.rs / effort.rs / permission.rs → claude_pty.rs journal()).
        tx.send(native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Configuration,
            "instance.configure",
            "not-applicable",
            &[],
            remuda_protocol::Severity::Error,
            false,
            "model-control-unavailable:could not write to control",
        ))
        .await
        .unwrap();
        tx.send(native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Configuration,
            "instance.configure",
            "not-applicable",
            &[],
            remuda_protocol::Severity::Error,
            false,
            "effort-control-unavailable:submit failed",
        ))
        .await
        .unwrap();

        // Give the pump time to process both; it must not have folded to failed
        // (no notification could ever prove the negative, so poll a few ticks).
        tokio::time::sleep(Duration::from_millis(80)).await;
        let after_configure = store.get_instance(&id).unwrap();
        assert!(
            matches!(after_configure.lifecycle, InstanceLifecycle::Ready),
            "a live configure error must not end the session: {:?}",
            after_configure.lifecycle
        );
        assert!(
            !matches!(
                after_configure.activity,
                Knowledge::Known {
                    value: Activity::WaitingInteraction
                }
            ),
            "the switch error does not block the live session"
        );

        // A REAL process death still ends the instance.
        tx.send(native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Session,
            "exit",
            "not-applicable",
            &[("lastError", "pane exited; agent process is gone")],
            remuda_protocol::Severity::Error,
            true,
            "pane exited; agent process is gone",
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.get_instance(&id).unwrap().lifecycle == InstanceLifecycle::Failed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("a genuine exit still fails the instance");

        drop(tx);
        pump.await.unwrap();
    }

    /// c-cardsettle r3 item 8 — the owner-reported sequence: a workflow
    /// subagent's failed stop (StopFailure, agentId=workflow-subagent,
    /// outcome=failed) followed by subagent start/stop markers keeps the MAIN
    /// instance running and usable. The failed outcome belongs to the
    /// subagent's turn only.
    #[tokio::test]
    async fn subagent_stopfailure_pump_keeps_the_main_instance_running() {
        use remuda_protocol::{Activity, DriverKind, Knowledge};
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
        let (tx, rx) = mpsc::channel(16);
        let pump = spawn_observation_pump(
            Arc::clone(&store),
            interactions,
            id.clone(),
            rx,
            Arc::new(FakeDriver::default()),
            Arc::new(crate::prompt_correlation::PromptCorrelator::default()),
        );

        // The subagent's failed stop (the bug event)…
        tx.send(native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "StopFailure",
            "not-applicable",
            &[
                ("agentId", "agent0sub0agent000"),
                ("agentType", "workflow-subagent"),
                ("outcome", "failed"),
                ("phase", "turn-ended"),
            ],
            remuda_protocol::Severity::Warning,
            false,
            "idle",
        ))
        .await
        .unwrap();
        // …and subagent start/stop markers for other workflow members.
        for (name, agent) in [
            ("SubagentStop", "agent0000000000000001"),
            ("SubagentStart", "agent0000000000000002"),
        ] {
            tx.send(native_lifecycle_full(
                remuda_protocol::LifecycleTopic::Diagnostic,
                name,
                "not-applicable",
                &[("agentId", agent), ("agentType", "workflow-subagent")],
                remuda_protocol::Severity::Info,
                false,
                "idle",
            ))
            .await
            .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(80)).await;
        let row = store.get_instance(&id).unwrap();
        assert!(
            !matches!(
                row.lifecycle,
                InstanceLifecycle::Failed | InstanceLifecycle::Exited
            ),
            "a subagent failure never ends the main instance: {:?}",
            row.lifecycle
        );

        // A subsequent MAIN-session StopFailure ends the TURN failed but still
        // never the process.
        tx.send(native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "StopFailure",
            "not-applicable",
            &[("outcome", "failed"), ("phase", "turn-ended")],
            remuda_protocol::Severity::Warning,
            false,
            "idle",
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let row = store.get_instance(&id).unwrap();
        assert!(
            !matches!(
                row.lifecycle,
                InstanceLifecycle::Failed | InstanceLifecycle::Exited
            ),
            "a main StopFailure ends the TURN, not the process: {:?}",
            row.lifecycle
        );

        // r4 item 2: a failed turn (topic=turn result error, even the final
        // result) does NOT end a LIVE child — the child accepts another
        // prompt and cards stay pending until a real topic=session exit.
        tx.send(native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "result",
            "not-applicable",
            &[("resultIndex", "1"), ("outcome", "failed")],
            remuda_protocol::Severity::Error,
            true,
            "error",
        ))
        .await
        .unwrap();
        // r5 item 4: the ROOT result error ENDS THE TURN — composer idle —
        // while the process stays alive.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let failed_turn = store.get_instance(&id).unwrap();
        assert!(
            !matches!(
                failed_turn.lifecycle,
                InstanceLifecycle::Failed | InstanceLifecycle::Exited
            ),
            "a result error never ends the process: {:?}",
            failed_turn.lifecycle
        );
        assert_eq!(
            failed_turn.activity,
            Knowledge::Known {
                value: Activity::Idle
            },
            "a root result error ends the turn failed (composer idle)"
        );
        // The live child is still alive enough to accept another prompt.
        tx.send(native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "UserPromptSubmit",
            "not-applicable",
            &[("phase", "prompt-accepted")],
            remuda_protocol::Severity::Info,
            false,
            "working",
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let row = store.get_instance(&id).unwrap();
        assert!(
            !matches!(
                row.lifecycle,
                InstanceLifecycle::Failed | InstanceLifecycle::Exited
            ),
            "a failed turn keeps the child alive for a retry: {:?}",
            row.lifecycle
        );

        drop(tx);
        pump.await.unwrap();
    }

    /// ma-sdk-state r4 item 2: on a structured print/sdk carrier a command's
    /// TRANSPORT acknowledgement is not turn evidence. The command completion
    /// is held until the turn_started observation has already applied
    /// working; the ack must leave the working state alone, and only the
    /// settled result idles. A PTY carrier keeps the old ack-idles behaviour.
    #[tokio::test]
    async fn structured_carrier_command_ack_keeps_turn_working_until_result() {
        use remuda_protocol::{Activity, DriverKind, InputOrigin, PromptMode};

        async fn wait_activity(
            store: &Arc<dyn LocalStore>,
            id: &InstanceId,
            want: Activity,
        ) -> Instance {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                let row = store.get_instance(id).unwrap();
                if let remuda_protocol::Knowledge::Known { value } = &row.activity
                    && *value == want
                {
                    return row;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("activity never reached {want:?}: {row:?}");
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        let send = DriverRequest::Send {
            prompt: "do the thing".to_owned(),
            attachments: Vec::new(),
            origin: InputOrigin::Human,
            mode: PromptMode::NewTurn,
        };

        // ── Structured sdk carrier ──────────────────────────────────────────
        let store: Arc<dyn LocalStore> = Arc::new(MemoryStore::new(64));
        let instance = fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ClaudeSdk,
        )
        .unwrap();
        let id = instance.meta.id.clone();
        store.insert_instance(instance).unwrap();
        let interactions = InteractionRuntime::spawn(Arc::clone(&store)).unwrap();
        let (tx, rx) = mpsc::channel(16);
        let pump = spawn_observation_pump(
            Arc::clone(&store),
            interactions,
            id.clone(),
            rx,
            Arc::new(FakeDriver::default()),
            Arc::new(crate::prompt_correlation::PromptCorrelator::default()),
        );

        // Hold the command completion until the start observation is applied.
        let mut started = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "turn_started",
            "sess-item2",
            &[("nativeClientMessageId", "msg-item2")],
            remuda_protocol::Severity::Info,
            false,
            "working",
        );
        started.source.driver_kind = DriverKind::ClaudeSdk;
        tx.send(started).await.unwrap();
        wait_activity(&store, &id, Activity::Working).await;

        // The write ack lands AFTER the start: the Send ack must not idle.
        finish_instance_operation(store.as_ref(), &id, DriverKind::ClaudeSdk, &send).unwrap();
        let row = store.get_instance(&id).unwrap();
        assert_eq!(
            row.activity,
            remuda_protocol::Knowledge::Known {
                value: Activity::Working
            },
            "the send ack must not overwrite the turn_started working state"
        );
        // Any other successful control ack during the turn is just as inert.
        finish_instance_operation(
            store.as_ref(),
            &id,
            DriverKind::ClaudeSdk,
            &DriverRequest::Cancel,
        )
        .unwrap();
        finish_instance_operation(
            store.as_ref(),
            &id,
            DriverKind::ClaudeSdk,
            &DriverRequest::Configure {
                model: Some("haiku".to_owned()),
                effort: None,
                effort_index: None,
                permission_mode: None,
            },
        )
        .unwrap();
        let row = store.get_instance(&id).unwrap();
        assert_eq!(
            row.activity,
            remuda_protocol::Knowledge::Known {
                value: Activity::Working
            },
            "cancel/configure acks keep the native turn state"
        );

        // Only the settled result ends the turn.
        let mut result = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Turn,
            "result",
            "sess-item2",
            &[
                ("resultIndex", "0"),
                ("queuedTurnCount", "0"),
                ("settledRootTurn", "true"),
            ],
            remuda_protocol::Severity::Info,
            false,
            "turn_done",
        );
        result.source.driver_kind = DriverKind::ClaudeSdk;
        tx.send(result).await.unwrap();
        let row = wait_activity(&store, &id, Activity::Idle).await;
        assert_eq!(
            row.lifecycle,
            InstanceLifecycle::Ready,
            "a turn result idles without ending the sdk process"
        );
        drop(tx);
        pump.await.unwrap();

        // ── PTY carrier keeps the ack-idles behaviour ───────────────────────
        let pty: Arc<dyn LocalStore> = Arc::new(MemoryStore::new(64));
        let pty_instance = fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ShellPty,
        )
        .unwrap();
        let pty_id = pty_instance.meta.id.clone();
        pty.insert_instance(pty_instance).unwrap();
        pty.set_instance_state(
            &pty_id,
            None,
            Some(remuda_protocol::Knowledge::Known {
                value: Activity::Working,
            }),
        )
        .unwrap();
        finish_instance_operation(pty.as_ref(), &pty_id, DriverKind::ShellPty, &send).unwrap();
        assert_eq!(
            pty.get_instance(&pty_id).unwrap().activity,
            remuda_protocol::Knowledge::Known {
                value: Activity::Idle
            },
            "a PTY carrier has no native turn lifecycle: its ack still idles"
        );
    }

    /// c-cardsettle r7 item 5 (was r5 item 3, strengthened): the owner's
    /// recorded RAW hook is mapped through the REAL `map_event` mapper and
    /// driven through the REAL Node pump — not a hand-built observation — with
    /// the root explicitly seeded WORKING (the old fixture defaulted to Idle,
    /// so an idle-stamping regression compared Idle to Idle and passed).
    ///
    /// 1. Root stays Working (lifecycle and activity literally unchanged)
    ///    after the subagent StopFailure carrying the recorded
    ///    remudaActivity=idle.
    /// 2. The workflow member is journaled Failed.
    /// 3. The Node's COMMITTED output is then fed into the HUB projection:
    ///    the pending approval stays pending and the member row is failed
    ///    there too.
    #[tokio::test]
    async fn owner_subagent_stopfailure_pump_keeps_working_root_fails_member_and_hub_keeps_card() {
        use remuda_protocol::{DriverKind, ObservationPayload, SourceChannel};
        use remuda_signal::{HookEvent, map_event};

        // On-disk session tree the tailer resolves runs from.
        let tmp = tempfile::TempDir::new().unwrap();
        let enc = tmp.path().join("enc");
        std::fs::create_dir_all(&enc).unwrap();
        std::fs::write(enc.join("sid.jsonl"), "{}\n").unwrap();
        let run_dir = enc.join("sid/subagents/workflows/wf_owner");
        std::fs::create_dir_all(&run_dir).unwrap();
        // The harness wrote the agent transcript at spawn; the hook names no
        // path (owner order: StopFailure beats SubagentStart).
        std::fs::write(run_dir.join("agent-agent0sub0agent000.jsonl"), "{}\n").unwrap();

        let store: Arc<dyn LocalStore> = Arc::new(MemoryStore::new(64));
        let instance = fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ShellPty,
        )
        .unwrap();
        let id = instance.meta.id.clone();
        let journal_id = instance.journal_id.clone();
        store.insert_instance(instance).unwrap();
        // Explicitly WORKING root — this is what made the old test vacuous.
        store
            .set_instance_state(
                &id,
                None,
                Some(remuda_protocol::Knowledge::Known {
                    value: Activity::Working,
                }),
            )
            .unwrap();
        let interactions = InteractionRuntime::spawn(Arc::clone(&store)).unwrap();
        let (tx, rx) = mpsc::channel(16);
        let pump = spawn_observation_pump(
            Arc::clone(&store),
            interactions,
            id.clone(),
            rx,
            Arc::new(FakeDriver::default()),
            Arc::new(crate::prompt_correlation::PromptCorrelator::default()),
        );

        const PPID: i32 = 4242;
        let transcript = enc.join("sid.jsonl").to_string_lossy().into_owned();

        /// Map a recorded raw hook through the REAL mapper, then stamp the
        /// envelope the production bus stamps (Hook / shell-pty).
        fn map_raw_hook(name: &str, payload: serde_json::Value) -> remuda_protocol::Observation {
            let mapped = map_event(&HookEvent {
                name: name.into(),
                ppid: PPID,
                payload,
            });
            let mut observation = remuda_protocol::Observation {
                schema_version: remuda_protocol::SchemaVersion,
                event_id: remuda_protocol::EventId::new(),
                journal_id: Id::new("obj").unwrap(),
                instance_id: InstanceId::new(),
                run_id: None,
                host_id: HostId::new(),
                process_generation: U64(1),
                run_generation: None,
                seq: U64(1),
                observed_at: timestamp_now().unwrap(),
                native_at: unknown("test"),
                source: crate::driver::runtime_source(
                    &fixture_instance(
                        InstanceId::new(),
                        HostId::new(),
                        WorkspaceId::new(),
                        DriverKind::ShellPty,
                    )
                    .unwrap(),
                    U64(1),
                ),
                completeness: mapped.completeness,
                raw_ref: None,
                evidence_event_ids: Vec::new(),
                body: mapped.payload,
            };
            observation.source.channel = SourceChannel::Hook;
            observation.source.driver_kind = DriverKind::ShellPty;
            observation
        }

        /// Merge a live-fold tag into a mapped native observation.
        fn tag(
            observation: &mut remuda_protocol::Observation,
            key: &'static str,
            value: &'static str,
        ) {
            let ObservationPayload::Lifecycle(payload) = &mut observation.body else {
                panic!("the mapper produced a lifecycle payload");
            };
            let remuda_protocol::LifecyclePayload::Native(native) = payload.as_mut() else {
                panic!("a native lifecycle payload");
            };
            native.related_ids.insert(key.into(), value.into());
        }

        // SessionStart arrives first; with no foreground reading yet it is
        // held PENDING (the honest "unverified binding" rule).
        let mut session_start = map_raw_hook(
            "SessionStart",
            serde_json::json!({
                "session_id": "sid",
                "transcript_path": transcript,
            }),
        );
        tag(&mut session_start, "remudaActivity", "working");
        tx.send(session_start).await.unwrap();

        // The promotion poll then identifies the foreground pid: the pending
        // SessionStart binds to it (the real production order). Without this,
        // owns_hook is false and the subagent activity guard is never
        // exercised.
        let mut promoted = native_lifecycle_full(
            remuda_protocol::LifecycleTopic::Session,
            "agent_promoted",
            "not-applicable",
            &[("kind", "claude"), ("pid", "4242")],
            remuda_protocol::Severity::Info,
            false,
            "observed",
        );
        promoted.source.channel = SourceChannel::Runtime;
        promoted.source.driver_kind = DriverKind::ShellPty;
        tx.send(promoted).await.unwrap();

        // The owner's first event: subagent StopFailure. Raw payload fields
        // are the harness's snake_case wire keys; the live fold's turn-ended
        // phase and the (old-shim) remudaActivity=idle tag are merged onto the
        // raw observation exactly like the recorded event.
        let mut stop_failure = map_raw_hook(
            "StopFailure",
            serde_json::json!({
                "session_id": "sid",
                "transcript_path": transcript,
                "agent_id": "agent0sub0agent000",
                "agent_type": "workflow-subagent",
                "outcome": "failed",
            }),
        );
        tag(&mut stop_failure, "phase", "turn-ended");
        tag(&mut stop_failure, "remudaActivity", "idle");
        tx.send(stop_failure).await.unwrap();

        // Poll until the member-failed observation lands (or time out).
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let page = store.read_events(&journal_id, None, 256).unwrap();
                let failed = page.events.iter().any(|event| {
                    let remuda_protocol::JournalEvent::Instance(event) = event else {
                        return false;
                    };
                    matches!(&event.body,
                        ObservationPayload::WorkflowMember(member)
                            if matches!(&member.native_agent_id,
                                remuda_protocol::Knowledge::Known { value }
                                    if value == "agent0sub0agent000")
                                && member.state == remuda_protocol::WorkflowState::Failed)
                });
                if failed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the failed workflow member is journaled");

        // The root turn is unchanged: lifecycle still Ready and, decisively,
        // activity still WORKING — a subagent remudaActivity=idle cannot idle
        // a working root (the pre-r7 fixture asserted Idle against Idle).
        let root = store.get_instance(&id).unwrap();
        assert_eq!(
            root.lifecycle,
            InstanceLifecycle::Ready,
            "a subagent StopFailure never ends the root"
        );
        assert_eq!(
            root.activity,
            remuda_protocol::Knowledge::Known {
                value: Activity::Working
            },
            "the subagent's remudaActivity=idle never moves a WORKING root"
        );

        // Collect the Node's COMMITTED output (every observation the pump
        // actually journaled, including the synthesized member-failed).
        let committed: Vec<remuda_protocol::Observation> = store
            .read_events(&journal_id, None, 256)
            .unwrap()
            .events
            .into_iter()
            .filter_map(|event| match event {
                remuda_protocol::JournalEvent::Instance(observation) => Some(*observation),
                _ => None,
            })
            .collect();
        assert!(
            committed.iter().any(|o| matches!(&o.body,
                ObservationPayload::WorkflowMember(m)
                    if m.state == remuda_protocol::WorkflowState::Failed)),
            "the committed output carries the failed member"
        );

        drop(tx);
        pump.await.unwrap();

        // ── Hub projection half ────────────────────────────────────────────
        // Feed the Node's committed output (the exact uplink conversion:
        // serde_json::to_value(JournalEvent::Instance)) into a real Hub Store
        // with a running, card-holding instance.
        let hub_dir = tempfile::tempdir().unwrap();
        let hub_store =
            remuda_hub::store_test_support::Store::open(hub_dir.path()).expect("hub store");
        let hub_host = remuda_protocol::HostId::new().as_id().to_string();
        let hub_instance = remuda_protocol::InstanceId::new().as_id().to_string();
        hub_store
            .ensure_instance(hub_host.clone(), hub_instance.clone())
            .await
            .unwrap();
        let hub_append = |event: serde_json::Value| {
            let hub_store = &hub_store;
            let host = hub_host.clone();
            let instance = hub_instance.clone();
            async move {
                hub_store
                    .append_journal(host, instance, None, event)
                    .await
                    .expect("hub append")
            }
        };
        hub_append(serde_json::json!({"kind":"lifecycle","payload":{
            "type":"entity","entityType":"instance","state":"ready"}}))
        .await;
        hub_append(serde_json::json!({"kind":"lifecycle","payload":{
            "type":"native","topic":"turn","nativeName":"agent_status",
            "status":{"state":"known","value":"working"}}}))
        .await;
        let hub_card = format!("int_{}", uuid::Uuid::now_v7());
        hub_append(
            serde_json::json!({"kind":"interaction.requested","payload":{
            "interactionKind":"approval",
            "interaction":{
                "id": hub_card, "kind":"approval", "state":"pending",
                "blocking": true, "answerable": true,
                "resolution": {"state":"unknown"},
                "request": {"kind":"approval","title":"Bash","description":"x","options":[]}}}}),
        )
        .await;

        // Re-stamp each Node observation onto the Hub instance and append in
        // journal order, exactly as the WSS uplink serializes it.
        for mut observation in committed {
            observation.instance_id =
                remuda_protocol::InstanceId::try_from(hub_instance.clone()).unwrap();
            let value = serde_json::to_value(remuda_protocol::JournalEvent::Instance(Box::new(
                observation,
            )))
            .expect("node journal event serializes to hub wire JSON");
            hub_append(value).await;
        }

        let hub_row = hub_store
            .get_instance(hub_instance.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            hub_row.lifecycle, "running",
            "the hub projection keeps the root running"
        );
        assert_ne!(
            hub_row.activity, "idle",
            "the subagent StopFailure never idles the working root at the hub either"
        );
        let pending = hub_store
            .list_interactions(None, Some(hub_instance.clone()), None, true)
            .await
            .unwrap();
        assert!(
            pending.iter().any(|r| r.interaction_id == hub_card),
            "the pending approval is still pending after the subagent failure"
        );
        // The member row is failed in the Hub journal projection (the wire
        // shape the uplink serializes: kind=workflow.member, state=failed,
        // nativeAgentId carrying the owner's subagent id).
        let hub_page = hub_store
            .read_journal(hub_instance.clone(), 0, None)
            .await
            .unwrap();
        let member_failed = hub_page.events.iter().any(|record| {
            let event = &record.event;
            event.get("kind").and_then(|v| v.as_str()) == Some("workflow.member")
                && event.pointer("/payload/state").and_then(|v| v.as_str()) == Some("failed")
                && event
                    .pointer("/payload/nativeAgentId/value")
                    .and_then(|v| v.as_str())
                    == Some("agent0sub0agent000")
        });
        assert!(
            member_failed,
            "the hub projection records the workflow member failed: {}",
            serde_json::to_value(&hub_page.events).unwrap_or_default()
        );
        hub_store.close().await;
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
    /// driver whose materialization parks (slow `pin_binary`, overlay/shim
    /// generation, PTY spawn) must not delay the create: the response lands
    /// with an accepted command and a `preparing` instance while the worker
    /// is still parked in `start`, and the worker walks the instance to
    /// `ready` once materialization is released.
    #[tokio::test]
    async fn create_is_accepted_before_a_slow_launch_finishes() {
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_roots(remuda_testing::test_workspace_roots!());
        let store = Arc::new(MemoryStore::new(8));
        let drivers = DriverRegistry::default();
        // The shared fake parks in `start()` until the test releases the
        // gate, so the accept-vs-launch ordering is observed as events
        // rather than raced against an elapsed-time threshold.
        let start_gate = Arc::new(tokio::sync::Notify::new());
        let slow = Arc::new(
            crate::FakeDriver::new(remuda_protocol::DriverKind::ClaudePrint)
                .with_start_gate(Arc::clone(&start_gate)),
        );
        drivers.register(slow).expect("register slow fake");
        let node = DevNode::with_parts(&config, store, drivers).expect("node");

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
        // The reply is the durable accept; the launch gate is still held.
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

        // While the gate is held the worker can never pass `start()`: this
        // is the deterministic form of "the ack did not wait for launch" —
        // a create that awaited materialization could never have returned,
        // because no one has released the gate yet.
        let parked = node
            .get_instance(&created.instance.meta.id)
            .expect("instance remains visible");
        assert!(
            matches!(
                parked.lifecycle,
                InstanceLifecycle::Preparing | InstanceLifecycle::Starting
            ),
            "instance must not pass start() while the gate is held: {:?}",
            parked.lifecycle
        );

        // Release materialization: the worker journals starting → ready.
        start_gate.notify_one();
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
                                value: "applied model=opus effort=ultracode index=3 permission=-"
                                    .into(),
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

    /// model-pin-1: the post-launch pin read-back gate.
    mod model_pin_gate {
        use super::*;

        const PIN: &str = "acme_hub/model_x_o50[1m]";

        /// The driver's synthetic launch snapshot: `source = Launch`, and crucially
        /// NO `raw` spelling, because the process has not spoken yet.
        fn launch_snapshot(id: &str, catalog: Option<Vec<String>>) -> remuda_protocol::Observation {
            model(id, remuda_protocol::EffortSource::Launch, None, catalog)
        }

        /// The first genuine launch read-back: `source = Launch` (the launch
        /// seeded attribution) but carrying the native transcript spelling in
        /// `raw`. An assistant record or a launch-settled `/model` verdict.
        fn launch_readback(id: &str) -> remuda_protocol::Observation {
            model(
                id,
                remuda_protocol::EffortSource::Launch,
                Some(id.to_owned()),
                None,
            )
        }

        /// A later, non-launch model edge (human `/model`, a Remuda switch, or
        /// an unnamed subsequent assistant record).
        fn later_edge(
            id: &str,
            source: remuda_protocol::EffortSource,
        ) -> remuda_protocol::Observation {
            model(id, source, Some(id.to_owned()), None)
        }

        #[allow(clippy::too_many_arguments)]
        fn model(
            id: &str,
            source: remuda_protocol::EffortSource,
            raw: Option<String>,
            catalog: Option<Vec<String>>,
        ) -> remuda_protocol::Observation {
            let mut observation = native_lifecycle(
                remuda_protocol::LifecycleTopic::Configuration,
                "envelope.only",
                "native-1",
                &[],
            );
            observation.body = ObservationPayload::Model(Box::new(remuda_protocol::ModelPayload {
                requested: None,
                effective: remuda_protocol::EffectiveModel {
                    id: id.to_owned(),
                    source,
                    observed_at: timestamp_now().expect("now"),
                },
                raw,
                catalog: catalog.map(|models| remuda_protocol::ModelCatalogInfo {
                    models,
                    source: remuda_protocol::ModelListSource::GatewayDiscovery,
                    observed_at: timestamp_now().expect("now"),
                    cache: None,
                    discovery_env: None,
                }),
                selection_path: None,
            }));
            observation
        }

        /// The substitution, on the launch read-back: a different id in the
        /// pin's own vocabulary answered. The gate reports it once, naming both
        /// ids verbatim; the pump (not asserted here) records and keeps running.
        #[test]
        fn a_substituted_model_on_the_launch_readback_reports_a_divergence_naming_both_ids() {
            let mut gate = ModelPinGate::new(Some(PIN.to_owned()));
            // The snapshot asserts the pin first; it must not settle anything.
            assert_eq!(gate.observe(&launch_snapshot(PIN, None)), None);
            assert!(!gate.settled);
            let divergence = gate
                .observe(&launch_readback("acme_hub/model_x_o48[1m]"))
                .expect("a namespaced substitution is a reported divergence");
            assert_eq!(divergence.pin, PIN);
            assert_eq!(divergence.observed, "acme_hub/model_x_o48[1m]");
            // One launch, one verdict: a later edge reports nothing more.
            assert_eq!(
                gate.observe(&launch_readback("acme_hub/model_x_o48[1m]")),
                None
            );
        }

        /// The measured false positive a byte-equality gate would have made: a
        /// correct gateway launch resolves the pin to an upstream vendor name.
        #[test]
        fn a_gateway_resolution_on_the_launch_readback_is_not_reported() {
            let mut gate = ModelPinGate::new(Some(PIN.to_owned()));
            assert_eq!(gate.observe(&launch_snapshot(PIN, None)), None);
            assert_eq!(gate.observe(&launch_readback("claude-opus-5")), None);
            assert!(
                gate.settled,
                "a real read-back is judged once even when it passes"
            );
        }

        /// The pin itself answering is silent.
        #[test]
        fn the_pin_answering_is_silent() {
            let mut gate = ModelPinGate::new(Some(PIN.to_owned()));
            assert_eq!(gate.observe(&launch_readback(PIN)), None);
            assert!(gate.settled);
        }

        /// The snapshot alone is never judged, even though it names a model.
        /// Judging it was the unfalsifiable hole in the first implementation.
        #[test]
        fn the_synthetic_snapshot_is_never_judged() {
            let mut gate = ModelPinGate::new(Some(PIN.to_owned()));
            assert_eq!(
                gate.observe(&launch_snapshot("acme_hub/model_x_o48[1m]", None)),
                None,
                "even a snapshot that names a different id is only a prediction"
            );
            assert!(!gate.settled);
        }

        /// After the launch read-back, a later switch is out of scope and cannot
        /// be reported — even to a different model.
        #[test]
        fn a_later_human_or_remuda_switch_is_never_reported() {
            let mut gate = ModelPinGate::new(Some(PIN.to_owned()));
            assert_eq!(gate.observe(&launch_readback(PIN)), None);
            assert_eq!(
                gate.observe(&later_edge(
                    "acme_hub/model_x_o48[1m]",
                    remuda_protocol::EffortSource::Slash
                )),
                None
            );
            assert_eq!(
                gate.observe(&later_edge(
                    "ark/model-y",
                    remuda_protocol::EffortSource::Remuda
                )),
                None
            );
        }

        /// Fail-open: no genuine read-back (no assistant message, no verdict)
        /// means no evidence, so nothing is recorded.
        #[test]
        fn with_no_read_back_there_is_no_report() {
            let mut gate = ModelPinGate::new(Some(PIN.to_owned()));
            assert_eq!(gate.observe(&launch_snapshot(PIN, None)), None);
            assert_eq!(
                gate.observe(&later_edge(
                    "acme_hub/model_x_o48[1m]",
                    remuda_protocol::EffortSource::Unknown
                )),
                None
            );
            assert!(!gate.settled);
        }

        /// A pin that was never requested cannot mismatch.
        #[test]
        fn no_pin_never_reports() {
            let mut gate = ModelPinGate::new(None);
            assert_eq!(
                gate.observe(&launch_readback("acme_hub/model_x_o48[1m]")),
                None
            );
        }

        /// The gate arms from the *explicit* pin, not the profile-derived
        /// `model_requested`. Blank/absent explicit pins arm no gate, even
        /// though an unpinned launch's `model_requested` still carries the
        /// profile default.
        #[test]
        fn an_unpinned_or_blank_pin_arms_no_gate() {
            assert_eq!(normalize_explicit_pin(None), None);
            assert_eq!(normalize_explicit_pin(Some("   ")), None);
            assert_eq!(normalize_explicit_pin(Some("")), None);
            assert_eq!(
                normalize_explicit_pin(Some("  acme_hub/x[1m] ")),
                Some("acme_hub/x[1m]".to_owned())
            );
            assert_eq!(pin_from_recipe(&None), None);
        }
        /// The catalog from the snapshot makes an un-namespaced launch read-back
        /// comparable, upgrading it to a reported divergence.
        #[test]
        fn a_catalog_makes_an_unnamespaced_substitution_decidable() {
            let mut gate = ModelPinGate::new(Some(PIN.to_owned()));
            let catalog = vec!["model_x_o48".to_owned(), "model_x_o50".to_owned()];
            gate.observe(&launch_snapshot(PIN, Some(catalog)));
            let divergence = gate
                .observe(&launch_readback("model_x_o48"))
                .expect("a catalog-listed substitution is decidable");
            assert_eq!(divergence.pin, PIN);
            assert_eq!(divergence.observed, "model_x_o48");
        }

        /// Non-model observations never decide anything.
        #[test]
        fn an_unrelated_observation_is_ignored() {
            let mut gate = ModelPinGate::new(Some(PIN.to_owned()));
            assert_eq!(
                gate.observe(&native_lifecycle(
                    remuda_protocol::LifecycleTopic::Configuration,
                    "something.else",
                    "native-1",
                    &[],
                )),
                None
            );
            assert!(!gate.settled);
        }
    }
}

#[cfg(test)]
mod api_relay_launch_test {
    use super::*;
    use crate::{FakeDriver, MemoryStore};

    fn node() -> DevNode {
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_roots(remuda_testing::test_workspace_roots!());
        let drivers = DriverRegistry::default();
        drivers
            .register(Arc::new(FakeDriver::default()))
            .expect("register fake driver");
        DevNode::with_parts(&config, Arc::new(MemoryStore::new(8)), drivers).expect("compose node")
    }

    /// A create carrying a via route provisions the loopback listener on the
    /// accept path and echoes the *resolved* route (hub-relay, since no
    /// endpoint is named). The echo rides the create response for the Hub to
    /// record — never a requested `auto` (D-035).
    #[tokio::test]
    async fn via_create_echoes_resolved_route_and_serves_loopback() {
        let node = node();
        let request: CreateInstanceRequest = serde_json::from_value(serde_json::json!({
            "kind": "claude",
            "driver": "claude-print",
            "providerOverlay": { "kind": "gateway", "baseUrl": "http://gateway.example/v1" },
            "apiRoute": {
                "mode": "via",
                "viaHostId": HostId::new(),
                "route": "auto"
            }
        }))
        .expect("request");
        let created = node.create_instance(request).await.expect("create");
        let route = created.api_route.expect("echoed route");
        assert!(route.is_via());
        assert_eq!(route.route, Some(remuda_protocol::ApiRouteKind::HubRelay));
        // The per-instance listener is registered and bound to loopback.
        let registry = node.api_relay();
        // Registry key is the allocated instance id.
        let instance_id = created.instance.meta.id.clone();
        // Internal lookup is exercised through a request instead: with no
        // carrier link attached the listener answers 503, which proves it is
        // live without exposing internals.
        assert_eq!(
            registry
                .instance_relay(instance_id.as_id().as_str())
                .map(|relay| relay.local_addr().ip().is_loopback()),
            Some(true)
        );
    }

    /// An idempotent retried create reuses the first attempt's listener: it
    /// must not bind a second one (which would orphan the first), and the
    /// single live listener is revoked when the instance exits.
    #[tokio::test]
    async fn retried_create_keeps_one_listener_and_exit_revokes_it() {
        let node = node();
        let registry = node.api_relay();
        let host = node.host().meta.id.clone();
        let instance_id = remuda_protocol::InstanceId::new();
        let instance_key = instance_id.as_id().to_string();
        let mut value = serde_json::json!({
            "kind": "claude",
            "driver": "claude-print",
            "instanceId": instance_key.clone(),
            "hostId": host,
            "providerOverlay": { "kind": "gateway", "baseUrl": "http://gateway.example/v1" },
            "apiRoute": {
                "mode": "via",
                "viaHostId": HostId::new(),
                "route": "hub-relay"
            }
        });
        // Probe the bound port from outside: 503 with no link proves liveness.
        async fn listener_status(node: &DevNode, key: &str) -> reqwest::StatusCode {
            let relay = node
                .api_relay()
                .instance_relay(key)
                .expect("registered listener");
            reqwest::Client::new()
                .get(format!("{}/models", relay.base_url()))
                .bearer_auth(relay.bearer_token())
                .send()
                .await
                .expect("connect")
                .status()
        }

        let first: CreateInstanceRequest = serde_json::from_value(value.clone()).expect("request");
        node.create_instance(first)
            .await
            .expect("first create accepted");
        assert!(registry.instance_relay(&instance_key).is_some());
        assert_eq!(
            listener_status(&node, &instance_key).await,
            503,
            "first listener is live"
        );

        // Fresh command id but the same client-allocated instance id: the
        // instance-id idempotency path, not a new bind.
        let command_key = remuda_protocol::CommandId::new().as_id().to_string();
        value["commandId"] = serde_json::json!(command_key);
        let mut second: CreateInstanceRequest = serde_json::from_value(value).expect("request");
        second.command_id = Some(remuda_protocol::CommandId::try_from(command_key).unwrap());
        let retried = node.create_instance(second).await.expect("retry accepted");
        assert!(retried.api_route.is_some(), "retry still echoes the route");
        // The retry carries a fresh command id; the reply's command must still
        // be durably accepted (the Hub node_accepted gate rejects a queued
        // command with no accepted journal row).
        assert_eq!(
            retried.command.state,
            remuda_protocol::CommandState::Accepted,
            "instance-id retry echoes an accepted command"
        );
        let ledger = node
            .get_command(&retried.command.command_id)
            .expect("the retry command id was accepted in the ledger");
        assert_eq!(ledger.state, remuda_protocol::CommandState::Accepted);
        assert!(
            registry.instance_relay(&instance_key).is_some(),
            "exactly one registered listener"
        );
        let relay = registry
            .instance_relay(&instance_key)
            .expect("registered listener");
        let listener_url = relay.base_url().to_owned();
        let listener_bearer = relay.bearer_token().to_owned();
        assert_eq!(
            listener_status(&node, &instance_key).await,
            503,
            "the same listener is still live"
        );

        // Instance exit (the worker's terminal block calls this) revokes the
        // bearer and shuts the listener; poll until the port is gone — axum's
        // graceful shutdown drains in-flight keep-alive connections, so the
        // exact frame is nondeterministic, but within a second every new
        // connection fails at the socket (not merely 403).
        registry.revoke_instance(&instance_key);
        let gone = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if registry.instance_relay(&instance_key).is_none()
                    && reqwest::Client::builder()
                        .pool_max_idle_per_host(0)
                        .build()
                        .unwrap()
                        .get(format!("{listener_url}/models"))
                        .bearer_auth(&listener_bearer)
                        .send()
                        .await
                        .is_err()
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "no live listener after revoke");
    }

    /// A Hub timeout-retry reusing the exact command id and payload races the
    /// first call through the multi-second route provision. The route probe
    /// stalls for its 3 s budget; the per-instance provision lock serializes
    /// probe+bind, so the loser waits and reuses the winner's listener, then
    /// loses the instance-row insert and answers with the winner's accepted
    /// command rather than propagate a Conflict. The assertions pin the
    /// observable outcomes (both Ok, same command, same single live listener
    /// and its same loopback URL/bearer) rather than TCP connection counts —
    /// reqwest itself opens multiple sockets per stalled request, so a
    /// connection counter is not a measure of how many provisions probed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_creates_with_one_command_id_both_return_the_accepted_one() {
        let node = std::sync::Arc::new(node());
        let host = node.host().meta.id.clone();
        let instance_id = remuda_protocol::InstanceId::new();
        let command_id = remuda_protocol::CommandId::new();

        // A TCP endpoint that accepts the connect but never answers: the
        // route:auto direct-net probe then waits its full 3 s request
        // timeout, guaranteeing the two calls overlap inside provisioning.
        // Accepted sockets are held open (dropping one would make reqwest
        // fail immediately instead of waiting the timeout).
        let stall = std::net::TcpListener::bind("127.0.0.1:0").expect("stall listener");
        let endpoint = format!("http://127.0.0.1:{}/", stall.local_addr().unwrap().port());
        let held: std::sync::Arc<std::sync::Mutex<Vec<std::net::TcpStream>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let held_clone = std::sync::Arc::clone(&held);
        let acceptor = stall.try_clone().expect("clone stall listener");
        std::thread::spawn(move || {
            while let Ok((stream, _)) = acceptor.accept() {
                held_clone.lock().unwrap().push(stream);
            }
        });

        let mk = || -> CreateInstanceRequest {
            serde_json::from_value(serde_json::json!({
                "kind": "claude",
                "driver": "claude-print",
                "instanceId": instance_id.as_id().to_string(),
                "commandId": command_id.as_id().to_string(),
                "hostId": host,
                "providerOverlay": { "kind": "gateway", "baseUrl": "http://gateway.example/v1" },
                "apiRelayEndpoint": endpoint,
                "apiRoute": {
                    "mode": "via",
                    "viaHostId": HostId::new(),
                    "route": "auto"
                }
            }))
            .expect("request")
        };

        // Both enter the accept path together; the second overlaps the
        // winner's provision and races the instance insert.
        let node_a = std::sync::Arc::clone(&node);
        let node_b = std::sync::Arc::clone(&node);
        let req_a = mk();
        let req_b = mk();
        let (a, b) = tokio::join!(
            tokio::spawn(async move { node_a.create_instance(req_a).await }),
            tokio::spawn(async move { node_b.create_instance(req_b).await }),
        );
        let a = a.expect("task").expect("first create Ok");
        let b = b
            .expect("task")
            .expect("raced duplicate create Ok, not Conflict");

        // Both replies identify the same accepted command and instance.
        assert_eq!(a.command.command_id, b.command.command_id);
        assert_eq!(a.instance.meta.id, b.instance.meta.id);
        for response in [&a, &b] {
            assert!(
                matches!(
                    response.command.state,
                    remuda_protocol::CommandState::Accepted
                        | remuda_protocol::CommandState::Settled
                ),
                "an idempotent reply carries an accepted-or-settled command, got {:?}",
                response.command.state
            );
            assert!(response.api_route.is_some(), "both echo the via route");
        }
        // The failed probe fell back to hub-relay.
        assert!(
            a.api_route
                .as_ref()
                .and_then(|route| route.route)
                .is_some_and(|kind| kind == remuda_protocol::ApiRouteKind::HubRelay)
        );
        // Exactly one live listener for the instance — the loser's
        // provisional bind (if any) was torn down, not left orphaned.
        let registry = node.api_relay();
        let relay = registry
            .instance_relay(instance_id.as_id().as_str())
            .expect("one registered listener");
        // The single surviving listener is live and answers authenticated
        // local traffic with the no-link 503; both replies describe that one.
        let status = reqwest::Client::new()
            .get(format!("{}/models", relay.base_url()))
            .bearer_auth(relay.bearer_token())
            .send()
            .await
            .expect("connect")
            .status();
        assert_eq!(status, 503);
        assert_eq!(
            a.api_route, b.api_route,
            "both echo the same observed route"
        );

        // Release the held sockets and stop accepting.
        drop(held);
        drop(stall);
    }

    /// Reusing a *terminated* (Exited) instance id with a fresh command id is
    /// not an idempotent replay: the create returns the store's Conflict, and
    /// because the fallback is scoped to live rows, the listener it bound for
    /// the refused attempt is revoked rather than left alive.
    #[tokio::test]
    async fn create_naming_a_terminated_instance_id_fails_and_revokes_bind() {
        let node = node();
        let host = node.host().meta.id.clone();
        let instance_id = remuda_protocol::InstanceId::new();
        let value = serde_json::json!({
            "kind": "claude",
            "driver": "claude-print",
            "instanceId": instance_id.as_id().to_string(),
            "hostId": host,
            "providerOverlay": { "kind": "gateway", "baseUrl": "http://gateway.example/v1" },
            "apiRoute": { "mode": "via", "viaHostId": HostId::new(), "route": "hub-relay" }
        });
        node.create_instance(serde_json::from_value(value.clone()).expect("request"))
            .await
            .expect("first create accepted");
        let registry = node.api_relay();
        assert!(
            registry
                .instance_relay(instance_id.as_id().as_str())
                .is_some()
        );

        // Mark the instance exited the way the terminal worker path does.
        journal_instance_phase(
            node.store(),
            &instance_id,
            InstanceLifecycle::Exited,
            "ready",
            "test-exit",
        )
        .expect("mark exited");
        registry.revoke_instance(instance_id.as_id().as_str());

        // Fresh command id on the same terminated instance id: provision
        // succeeds (bind), but the insert conflict must propagate, and the
        // dropped guard must revoke the new listener.
        let mut retry: CreateInstanceRequest = serde_json::from_value(value).expect("request");
        retry.command_id = Some(remuda_protocol::CommandId::new());
        let error = node
            .create_instance(retry)
            .await
            .expect_err("terminated instance id is a Conflict");
        assert!(matches!(error, NodeError::Conflict(_)), "{error:?}");
        assert!(
            registry
                .instance_relay(instance_id.as_id().as_str())
                .is_none(),
            "the refused attempt's listener is revoked"
        );
    }

    /// A nameless `via` on the request rejects the create before any row or
    /// launch (mirrors the wire-level rule; the Node must not echo mode
    /// `via` without a host).
    #[tokio::test]
    async fn nameless_via_is_refused_before_launch() {
        let node = node();
        let request: CreateInstanceRequest = serde_json::from_value(serde_json::json!({
            "kind": "claude",
            "driver": "claude-print",
            "providerOverlay": { "kind": "gateway", "baseUrl": "http://gateway.example/v1" },
            "apiRoute": { "mode": "via", "route": "hub-relay" }
        }))
        .expect("request parses (wire validation lives on the Hub projector)");
        let error = node.create_instance(request).await.expect_err("refused");
        assert!(error.to_string().contains("viaHostId"), "{error}");
    }

    /// D-035 at the launch boundary: `route: direct-net` whose probe fails is
    /// refused with the stable `api-via-unreachable` code. Nothing is bound,
    /// no instance row exists, and there is no silent reroute to hub-relay.
    #[tokio::test]
    async fn direct_net_probe_failure_refuses_without_creating_an_instance() {
        let node = node();
        let instance_id = remuda_protocol::InstanceId::new();
        let request: CreateInstanceRequest = serde_json::from_value(serde_json::json!({
            "kind": "claude",
            "driver": "claude-print",
            "instanceId": instance_id.as_id().to_string(),
            "providerOverlay": { "kind": "gateway", "baseUrl": "http://gateway.example/v1" },
            // Port 1 refuses immediately, so the launch-time probe fails fast.
            "apiRelayEndpoint": "http://127.0.0.1:1/",
            "apiRoute": {
                "mode": "via",
                "viaHostId": HostId::new(),
                "route": "direct-net"
            }
        }))
        .expect("request");
        let error = node
            .create_instance(request)
            .await
            .expect_err("direct-net must refuse when the probe fails");
        assert!(
            error.to_string().contains("api-via-unreachable"),
            "the refusal names the stable code: {error}"
        );
        // No listener was bound for an instance that never launched.
        assert!(
            node.api_relay()
                .instance_relay(instance_id.as_id().as_str())
                .is_none(),
            "a refused direct-net launch must leave no listener behind"
        );
        // And no instance row: the refusal precedes the create insert.
        assert!(
            node.store().get_instance(&instance_id).ok().is_none(),
            "a refused launch creates no instance record"
        );
    }

    /// c-cardsettle r7 item 7 (OA6) tests: the two Node sites that used to end an
    /// instance on non-process-end evidence — a failed native-observation commit
    /// (the Node's own store failing) and a rejected non-PTY command (control /
    /// API error while the child may be alive). A scripted non-PTY driver keeps an
    /// event channel (driven through the REAL observation pump) and knobs for
    /// command failure and the driver's own child-gone report.
    #[cfg(test)]
    mod oa6_store_and_rejection_tests {
        use super::*;
        use crate::{
            DriverEmission, DriverError, DriverFuture, DriverLaunch, DriverRegistry, DriverRequest,
            MemoryStore,
        };
        use remuda_protocol::{
            Activity, DriverKind, Knowledge, LifecyclePayload, LifecycleTopic, MessagePhase,
            MessageRole, NativeLifecycle, Observation, ObservationPayload, Severity, SourceChannel,
        };
        use serde_json::json;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use tokio::sync::mpsc;

        /// Local copy of the shared test builder (this module is a sibling of
        /// `runtime::tests`, which keeps that helper private).
        fn native(
            topic: LifecycleTopic,
            name: &str,
            native_id: &str,
            severity: Severity,
            affects_completion: bool,
            status: &str,
        ) -> Observation {
            Observation {
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
                        DriverKind::ClaudePrint,
                    )
                    .expect("fixture instance"),
                    U64(1),
                ),
                completeness: Completeness::Structured,
                raw_ref: None,
                evidence_event_ids: Vec::new(),
                body: ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                    NativeLifecycle {
                        topic,
                        native_name: name.to_owned(),
                        native_id: Knowledge::Known {
                            value: native_id.to_owned(),
                        },
                        status: Knowledge::Known {
                            value: status.to_owned(),
                        },
                        related_ids: std::collections::BTreeMap::new(),
                        data_ref: None,
                        severity,
                        affects_completion,
                    },
                )))),
            }
        }

        /// Test-side controls for the one instance-local scripted driver.
        #[derive(Clone)]
        struct ScriptHandle {
            tx: Arc<Mutex<Option<mpsc::Sender<Observation>>>>,
            /// Next N `Send` commands fail with a control error (child alive).
            reject_sends: Arc<AtomicUsize>,
            /// What the driver reports from `process_gone`.
            gone: Arc<AtomicBool>,
            /// Every prompt the execute path actually accepted.
            sent: Arc<Mutex<Vec<String>>>,
        }

        struct ScriptPrintDriver {
            instance: Instance,
            handle: ScriptHandle,
        }

        impl Driver for ScriptPrintDriver {
            fn kind(&self) -> DriverKind {
                DriverKind::ClaudePrint
            }

            fn start(&self) -> crate::DriverStartFuture<'_> {
                // The real production pump consumes this channel: observations
                // sent here go through spawn_observation_pump exactly like the
                // print driver's stdout reader.
                Box::pin(async move {
                    let (tx, rx) = mpsc::channel(32);
                    *self.handle.tx.lock().unwrap() = Some(tx);
                    Ok(Some(rx))
                })
            }

            fn process_gone(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
                let gone = self.handle.gone.clone();
                Box::pin(async move { gone.load(Ordering::SeqCst) })
            }

            fn execute(&self, request: DriverRequest) -> DriverFuture<'_> {
                Box::pin(async move {
                    match request {
                        DriverRequest::Send { prompt, .. } => {
                            if self
                                .handle
                                .reject_sends
                                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                                    if n > 0 { Some(n - 1) } else { None }
                                })
                                .is_ok()
                            {
                                // A control/API rejection: the child is still alive.
                                return Err(DriverError::Failed(
                                    "scripted control rejection".into(),
                                ));
                            }
                            let mut emissions = Vec::new();
                            if prompt.contains("can_use_tool") {
                                let interaction =
                                    crate::interactions::fake_can_use_tool(&self.instance)
                                        .map_err(|error| DriverError::Failed(error.to_string()))?;
                                emissions.push(DriverEmission::InteractionRequested {
                                    interaction: Box::new(interaction),
                                });
                            }
                            self.handle.sent.lock().unwrap().push(prompt);
                            emissions.push(DriverEmission::Message {
                                role: MessageRole::Assistant,
                                phase: MessagePhase::Final,
                                text: "PONG".into(),
                            });
                            Ok(emissions)
                        }
                        // Close always works so teardown never errors.
                        DriverRequest::Close => Ok(Vec::new()),
                        other => {
                            // No other request type is used by these tests.
                            Err(DriverError::Failed(format!(
                                "scripted driver does not handle {other:?}"
                            )))
                        }
                    }
                })
            }
        }

        struct ScriptPrintFactory {
            handle: ScriptHandle,
        }

        impl crate::DriverFactory for ScriptPrintFactory {
            fn kind(&self) -> DriverKind {
                DriverKind::ClaudePrint
            }

            fn build(&self, launch: DriverLaunch) -> Result<Arc<dyn Driver>, DriverError> {
                Ok(Arc::new(ScriptPrintDriver {
                    instance: launch.instance,
                    handle: self.handle.clone(),
                }))
            }
        }

        fn new_handle() -> ScriptHandle {
            ScriptHandle {
                tx: Arc::new(Mutex::new(None)),
                reject_sends: Arc::new(AtomicUsize::new(0)),
                gone: Arc::new(AtomicBool::new(false)),
                sent: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn scripted_node() -> (DevNode, Arc<MemoryStore>, tempfile::TempDir, ScriptHandle) {
            let config = crate::DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!());
            let handle = new_handle();
            let registry = DriverRegistry::default();
            registry
                .register_factory(Arc::new(ScriptPrintFactory {
                    handle: handle.clone(),
                }))
                .expect("register scripted print factory");
            // DURABLE store: pending cards live in the EntityDb, so the test can
            // assert the card survives (and is not retired) via the same rows the
            // hello inventory lists.
            let data = tempfile::tempdir().expect("data dir");
            let store =
                Arc::new(MemoryStore::open_journaled(data.path(), 64).expect("durable store"));
            let node = DevNode::with_parts(&config, store.clone() as Arc<dyn LocalStore>, registry)
                .expect("compose node");
            (node, store, data, handle)
        }

        async fn wait_for(deadline_ms: u64, mut predicate: impl FnMut() -> bool) {
            tokio::time::timeout(Duration::from_millis(deadline_ms), async {
                while !predicate() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("condition in time");
        }

        async fn create_ready(node: &DevNode) -> Instance {
            let created = node
                .create_instance(
                    serde_json::from_value(json!({
                        "kind": "claude",
                        "driver": "claude-print",
                        "model": "fake",
                        "prompt": "first",
                    }))
                    .expect("request"),
                )
                .await
                .expect("create");
            let id = created.instance.meta.id.clone();
            wait_for(3000, || {
                node.get_instance(&id)
                    .is_ok_and(|instance| matches!(instance.lifecycle, InstanceLifecycle::Ready))
            })
            .await;
            wait_for(3000, || {
                node.get_command(&created.command.command_id)
                    .is_ok_and(|command| matches!(command.state, CommandState::Settled))
            })
            .await;
            node.get_instance(&id).expect("instance")
        }

        async fn send(node: &DevNode, id: &InstanceId, prompt: &str) -> Command {
            node.submit_command(
                id,
                serde_json::from_value(json!({ "operation": "send", "prompt": prompt }))
                    .expect("request"),
            )
            .await
            .expect("submit")
            .command
        }

        /// Send a native observation over the driver's REAL pump channel.
        fn pump_native(
            handle: &ScriptHandle,
            topic: LifecycleTopic,
            name: &str,
            status: &str,
            severity: Severity,
            channel: SourceChannel,
            kind: DriverKind,
        ) {
            let mut observation = native(topic, name, "sess-r7", severity, false, status);
            observation.source.channel = channel;
            observation.source.driver_kind = kind;
            handle
                .tx
                .lock()
                .unwrap()
                .as_ref()
                .expect("driver started its observation stream")
                .try_send(observation)
                .expect("pump channel accepts the observation");
        }

        fn events_named(node: &DevNode, instance: &Instance, name: &str) -> bool {
            node.read_journal(&instance.journal_id, None, 256)
                .expect("journal")
                .events
                .iter()
                .any(|event| match event {
                    JournalEvent::Instance(observation) => matches!(&observation.body,
                    ObservationPayload::Lifecycle(payload)
                        if matches!(payload.as_ref(),
                            remuda_protocol::LifecyclePayload::Native(native)
                                if native.native_name == name)),
                    _ => false,
                })
        }

        /// Site 1: the Node's own store rejects an observation commit while the
        /// child is alive. The instance stays live, its pending card stays
        /// pending, and the driver's later REAL exit still ends the instance.
        #[tokio::test]
        async fn observation_commit_failure_keeps_the_live_instance_and_its_card() {
            let (node, store, _data, handle) = scripted_node();

            let instance = create_ready(&node).await;
            let id = instance.meta.id.clone();

            // A pending approval card reaches the broker through the command path.
            let request = send(&node, &id, "please can_use_tool now").await;
            assert!(
                matches!(
                    request.state,
                    CommandState::Accepted | CommandState::Settled
                ),
                "the card-producing send reached the driver: {:?}",
                request.state
            );
            wait_for(3000, || {
                store
                    .pending_interactions()
                    .is_ok_and(|rows| rows.len() == 1)
            })
            .await;

            // The Node's own store now fails the next driver observation commit.
            store.fail_next_driver_observations(2);
            pump_native(
                &handle,
                LifecycleTopic::Diagnostic,
                "r7-commit-probe",
                "ok",
                Severity::Info,
                SourceChannel::Stdout,
                DriverKind::ClaudePrint,
            );

            // The fallback ERROR OBSERVATION (not a task exit) is journaled.
            wait_for(3000, || {
                events_named(&node, &instance, "observation-commit-failed")
            })
            .await;

            // The failure happened and the instance is still live — not failed,
            // not exited, no task-exit last_error, and its card still pending.
            let after = node.get_instance(&id).expect("instance");
            assert!(
                matches!(after.lifecycle, InstanceLifecycle::Ready),
                "a Node store failure never ends the child: {:?}",
                after.lifecycle
            );
            assert!(after.last_error.is_none(), "no task-exit last_error");
            assert_eq!(store.pending_interactions().unwrap().len(), 1);

            // The driver's OWN real exit (the exact print session/exited shape)
            // still ends the instance through the normal pump path; the entity
            // event the Hub settles cards from is journaled.
            pump_native(
                &handle,
                LifecycleTopic::Session,
                "session",
                "exited",
                Severity::Info,
                SourceChannel::Stdout,
                DriverKind::ClaudePrint,
            );
            wait_for(3000, || {
                node.get_instance(&id)
                    .is_ok_and(|instance| matches!(instance.lifecycle, InstanceLifecycle::Exited))
            })
            .await;

            node.shutdown().await.expect("shutdown");
        }

        /// Site 2: a rejected non-PTY command while the child is alive rejects the
        /// command only — the instance stays live and its card stays pending. A
        /// rejection the driver pairs with a confirmed child-gone DOES end it; and
        /// an alive instance still ends on the driver's own real exit.
        #[tokio::test]
        async fn command_rejection_keeps_the_live_instance_until_the_child_is_gone() {
            let (node, store, _data, handle) = scripted_node();
            let instance = create_ready(&node).await;
            let id = instance.meta.id.clone();

            let request = send(&node, &id, "please can_use_tool now").await;
            assert!(
                matches!(
                    request.state,
                    CommandState::Accepted | CommandState::Settled
                ),
                "the card-producing send reached the driver: {:?}",
                request.state
            );
            wait_for(3000, || {
                store
                    .pending_interactions()
                    .is_ok_and(|rows| rows.len() == 1)
            })
            .await;
            // Working root turn so a wrong idle/failed stamp would be visible.
            store
                .set_instance_state(
                    &id,
                    None,
                    Some(Knowledge::Known {
                        value: Activity::Working,
                    }),
                )
                .expect("seed working");

            // The next send is rejected while the driver reports the child ALIVE.
            handle.reject_sends.store(1, Ordering::SeqCst);
            let rejected = send(&node, &id, "retry after the error").await;
            // The worker settles asynchronously; wait for the rejected outcome.
            wait_for(3000, || {
                node.get_command(&rejected.command_id)
                    .is_ok_and(|command| matches!(command.state, CommandState::Settled))
            })
            .await;
            let rejected = node.get_command(&rejected.command_id).expect("command");
            assert_eq!(
                rejected.state,
                CommandState::Settled,
                "a rejected command still settles its ledger row"
            );
            assert!(
                matches!(&rejected.settlement,
                Knowledge::Known { value } if value.outcome == SettlementOutcome::Rejected),
                "the settlement records the rejection: {:?}",
                rejected.settlement
            );

            // Give the worker a tick; the instance must not have ended.
            tokio::time::sleep(Duration::from_millis(80)).await;
            let after = node.get_instance(&id).expect("instance");
            assert!(
                matches!(after.lifecycle, InstanceLifecycle::Ready),
                "a command rejection with the child alive never ends the instance: {:?}",
                after.lifecycle
            );
            assert_eq!(
                after.activity,
                Knowledge::Known {
                    value: Activity::Working
                },
                "a rejection does not touch activity"
            );
            assert!(after.last_error.is_none(), "no task-exit last_error");
            assert_eq!(store.pending_interactions().unwrap().len(), 1);
            // The rejection diagnostic is recorded.
            assert!(events_named(&node, &instance, "fake-driver-error"));

            // The driver's real exit still settles it.
            pump_native(
                &handle,
                LifecycleTopic::Session,
                "session",
                "exited",
                Severity::Info,
                SourceChannel::Stdout,
                DriverKind::ClaudePrint,
            );
            wait_for(3000, || {
                node.get_instance(&id)
                    .is_ok_and(|instance| matches!(instance.lifecycle, InstanceLifecycle::Exited))
            })
            .await;
            node.shutdown().await.expect("shutdown");

            // A rejection the driver pairs with a confirmed child-gone DOES end
            // the instance (the guard still honors real process-end evidence).
            let (node2, _store2, _data2, handle2) = scripted_node();
            let instance2 = create_ready(&node2).await;
            let id2 = instance2.meta.id.clone();
            handle2.gone.store(true, Ordering::SeqCst);
            handle2.reject_sends.store(1, Ordering::SeqCst);
            let _ = send(&node2, &id2, "send right as the child exits").await;
            wait_for(3000, || {
                node2
                    .get_instance(&id2)
                    .is_ok_and(|instance| matches!(instance.lifecycle, InstanceLifecycle::Failed))
            })
            .await;
            node2.shutdown().await.expect("shutdown");
        }
    }
}
