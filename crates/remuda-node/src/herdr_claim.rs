//! D-062: claim an external herdr pane as an Instance (`herdr-claimed`).
//!
//! Design: `docs/design/herdr-claim.md`. A human started the pane in another
//! herdr session on this host; the Node adopts it so the Hub can show and drive
//! it, but a claim is **not** ownership:
//!
//! * release/close only detaches — the pane is never closed, its process never
//!   signalled and its server never stopped;
//! * claimed panes are never `pty_resources` rows, so the orphan sweep, the stop
//!   ladder, `reclaim_instance_carriers` and carrier recovery cannot reach them;
//! * pane gone or process gone is process-end evidence; an unreachable server
//!   is contact loss and never ends the instance;
//! * a Node shutdown suspends the watcher without settling the row, and the
//!   next start re-attaches it ([`DevNode::reconcile_herdr_claims`]).
//!
//! Logs never carry pane content: discovery reads metadata only, and the one
//! screen read ([`ClaimedPane::screen`]) serves `tty.screen` without logging.

use crate::{
    DevNode, LocalStore, NodeError, TtyRegistry,
    driver::{Driver, DriverError, DriverFuture, DriverRequest},
    runtime::{append_instance_lifecycle, fixture_instance_with_session},
    store::unknown,
};
use remuda_herdr::{
    AgentPromptParams, AgentStatus, Client, PaneProcessInfo, PaneReadParams, ReadFormat,
    ReadSource, SessionSnapshot,
};
use remuda_protocol::hubnode::{
    HERDR_CLAIM_AMBIGUOUS, HERDR_CLAIM_CONFLICT, HERDR_CLAIM_INVALID, HERDR_CLAIM_OWN_SESSION,
    HERDR_UNREACHABLE, HerdrClaimResolution, HerdrClaimTarget, HerdrDiscoverParams,
    HerdrDiscoverResult, HerdrPaneListing, HerdrSessionListing, HerdrTabListing,
    HerdrWorkspaceListing, InstanceClaimParams, METHOD_HERDR_DISCOVER, METHOD_HERDR_RESOLVE,
    METHOD_INSTANCE_CLAIM,
};
use remuda_protocol::{
    Activity, AgentKind, Completeness, DriverKind, HerdrClaim, HerdrClaimProcess, Instance,
    InstanceId, InstanceLifecycle, Knowledge, LaunchedBy, LifecyclePayload, LifecycleTopic,
    NativeLifecycle, ObservationPayload, Ownership, Severity, U64, WorkspaceId,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Bound on every herdr RPC a claim makes: a wedged external server must not
/// wedge discovery, a claim, a watcher tick or a command.
const RPC_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a live claim is probed (pane present, process alive, status).
const DEFAULT_POLL: Duration = Duration::from_secs(2);
/// Discovery bounds: sessions per host and panes per session.
const MAX_SESSIONS: usize = 64;
const MAX_PANES: usize = 256;

/// Reason code when the claimed pane left its session (process-end evidence).
pub const REASON_PANE_GONE: &str = "herdr-pane-gone";
/// Reason code when the claimed process ended (process-end evidence).
pub const REASON_PROCESS_GONE: &str = "herdr-process-gone";
/// Reason code of a release (detach; the pane keeps running).
pub const REASON_RELEASED: &str = "released";
/// Native diagnostic carrying `lost` / `ok` contact with the herdr server.
pub const CONTACT_EVENT: &str = "herdr_claim_contact";

/// Whether `method` is one of the D-062 claim RPCs.
pub(crate) fn is_herdr_claim_method(method: &str) -> bool {
    matches!(
        method,
        METHOD_HERDR_DISCOVER | METHOD_HERDR_RESOLVE | METHOD_INSTANCE_CLAIM
    )
}

/// Node-wide claim settings and the live claims' handles.
pub(crate) struct ClaimRegistry {
    /// herdr config dir override (tests); `None` reads `$XDG_CONFIG_HOME/herdr`.
    root: std::sync::RwLock<Option<PathBuf>>,
    poll: std::sync::RwLock<Duration>,
    /// Serializes claims so the uniqueness check and the insert are atomic.
    lock: tokio::sync::Mutex<()>,
    live: std::sync::Mutex<BTreeMap<InstanceId, Arc<ClaimedPane>>>,
}

impl Default for ClaimRegistry {
    fn default() -> Self {
        Self {
            root: std::sync::RwLock::new(None),
            poll: std::sync::RwLock::new(DEFAULT_POLL),
            lock: tokio::sync::Mutex::new(()),
            live: std::sync::Mutex::new(BTreeMap::new()),
        }
    }
}

/// A herdr session the Node may talk to on a claim's behalf.
#[derive(Debug, Clone)]
struct SessionAddr {
    /// As the operator named it (`default`, a name, or a socket path).
    session: String,
    socket: PathBuf,
}

/// One probe of a live claim.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    /// The server did not answer: contact loss, never process end.
    Unreachable(String),
    /// Process-end evidence: reason code and a sentence naming it.
    Gone {
        reason: &'static str,
        evidence: String,
    },
    /// Pane present, process alive.
    Alive { status: AgentStatus },
}

impl DevNode {
    /// Point claim discovery at another herdr config dir. Test seam: the
    /// workspace forbids process-env mutation, so tests cannot move
    /// `$XDG_CONFIG_HOME`.
    #[doc(hidden)]
    pub fn set_herdr_claim_root(&self, root: PathBuf) {
        if let Ok(mut slot) = self.inner.herdr_claims.root.write() {
            *slot = Some(root);
        }
    }

    /// Probe interval for live claims. Test seam (production: 2 s).
    #[doc(hidden)]
    pub fn set_herdr_claim_poll(&self, poll: Duration) {
        if let Ok(mut slot) = self.inner.herdr_claims.poll.write() {
            *slot = poll;
        }
    }

    /// Dispatch one D-062 claim RPC (`herdr.discover`, `herdr.resolve`,
    /// `instance.claim`).
    pub async fn herdr_claim_rpc(&self, method: &str, params: Value) -> Result<Value, NodeError> {
        match method {
            METHOD_HERDR_DISCOVER => {
                let params: HerdrDiscoverParams = if params.is_null() {
                    HerdrDiscoverParams::default()
                } else {
                    serde_json::from_value(params)?
                };
                Ok(serde_json::to_value(self.herdr_discover(params).await?)?)
            }
            METHOD_HERDR_RESOLVE => {
                let target: HerdrClaimTarget = serde_json::from_value(params)?;
                Ok(serde_json::to_value(
                    self.herdr_resolve(&target, None).await?,
                )?)
            }
            METHOD_INSTANCE_CLAIM => {
                let params: InstanceClaimParams = serde_json::from_value(params)?;
                let instance = self.claim_instance(params).await?;
                Ok(json!({ "instance": instance }))
            }
            other => Err(NodeError::InvalidRequest(format!(
                "unknown herdr claim method {other}"
            ))),
        }
    }

    /// List reachable herdr sessions → workspaces → tabs → panes. Metadata
    /// only: never `pane.read` / `agent.read`. The Node's own session is not
    /// listed.
    pub async fn herdr_discover(
        &self,
        params: HerdrDiscoverParams,
    ) -> Result<HerdrDiscoverResult, NodeError> {
        let addrs = match params.session.as_deref() {
            Some(session) => vec![self.resolve_session(session)?],
            None => self.scan_sessions(),
        };
        let claimed = self.live_claims()?;
        let mut sessions = Vec::with_capacity(addrs.len());
        for addr in addrs {
            sessions.push(list_session(&addr, &claimed).await);
        }
        Ok(HerdrDiscoverResult { sessions })
    }

    /// Validate one claim target and say what a claim would record. Read-only.
    pub async fn herdr_resolve(
        &self,
        target: &HerdrClaimTarget,
        claiming: Option<&InstanceId>,
    ) -> Result<HerdrClaimResolution, NodeError> {
        let addr = self.resolve_session(&target.session)?;
        let client = Client::connect(&addr.socket).with_timeout(RPC_TIMEOUT);
        let snapshot = client
            .session_snapshot()
            .await
            .map_err(|error| unreachable_error(&addr, &error))?;
        let workspace_id = target.workspace_id.trim();
        let tab_id = target.tab_id.trim();
        if !snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.workspace_id == workspace_id)
        {
            return Err(invalid(format!(
                "workspace {workspace_id} is not in herdr session {}",
                addr.session
            )));
        }
        let tab = snapshot
            .tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or_else(|| {
                invalid(format!(
                    "tab {tab_id} is not in herdr session {}",
                    addr.session
                ))
            })?;
        if tab.workspace_id != workspace_id {
            return Err(invalid(format!(
                "tab {tab_id} belongs to workspace {}, not {workspace_id}",
                tab.workspace_id
            )));
        }
        let mut panes: Vec<_> = snapshot
            .panes
            .iter()
            .filter(|pane| pane.tab_id == tab_id && pane.workspace_id == workspace_id)
            .collect();
        panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        let pane = match target
            .pane_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(pane_id) => match panes.iter().find(|pane| pane.pane_id == pane_id) {
                Some(pane) => *pane,
                None => {
                    let elsewhere = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id);
                    return Err(invalid(match elsewhere {
                        Some(pane) => format!(
                            "pane {pane_id} belongs to tab {}, not {tab_id}",
                            pane.tab_id
                        ),
                        None => format!("pane {pane_id} is not in herdr session {}", addr.session),
                    }));
                }
            },
            None => match panes.as_slice() {
                [only] => *only,
                [] => return Err(invalid(format!("tab {tab_id} has no panes"))),
                many => {
                    let ids: Vec<&str> = many.iter().map(|pane| pane.pane_id.as_str()).collect();
                    return Err(NodeError::InvalidRequest(format!(
                        "{HERDR_CLAIM_AMBIGUOUS}: tab {tab_id} has {} panes; pass --pane with one of: {}",
                        ids.len(),
                        ids.join(", ")
                    )));
                }
            },
        };
        let pane_id = pane.pane_id.clone();
        let agent_row = snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id);
        let agent = pane
            .agent
            .clone()
            .or_else(|| agent_row.and_then(|row| row.agent.clone()));
        let status = agent_row.map_or(pane.agent_status, |row| row.agent_status);
        let kind = claim_kind(target.kind.as_deref(), agent.as_deref())?;
        let info = client
            .pane_process_info(Some(pane_id.clone()))
            .await
            .map_err(|error| unreachable_error(&addr, &error))?
            .process_info;
        let mut process = info.as_ref().and_then(|info| claim_process(info, kind));
        if let Some(process) = process.as_mut() {
            process.start_ticks = kernel_start_ticks(process.pid).map(U64);
        }
        let socket_path = canonical_string(&addr.socket);
        if let Some(owner) = self.live_claim_of(&socket_path, &pane_id, claiming)? {
            return Err(NodeError::Conflict(format!(
                "{HERDR_CLAIM_CONFLICT}: pane {pane_id} in herdr session {} is already claimed by {}",
                addr.session,
                owner.as_id()
            )));
        }
        Ok(HerdrClaimResolution {
            claim: HerdrClaim {
                session: addr.session,
                socket_path,
                workspace_id: workspace_id.to_owned(),
                tab_id: tab_id.to_owned(),
                pane_id,
                agent,
                process,
                contact: Some("ok".to_owned()),
            },
            kind,
            agent_status: status_word(status).to_owned(),
            cwd: pane.cwd.clone(),
        })
    }

    /// Claim an external herdr pane as `params.instance_id`. Idempotent per
    /// instance id. The returned Instance is live (`ready`).
    pub async fn claim_instance(&self, params: InstanceClaimParams) -> Result<Instance, NodeError> {
        let _mutation = self.inner.mutations.read().await;
        if self.inner.stopping.load(Ordering::SeqCst) {
            return Err(NodeError::InvalidRequest("Node is shutting down".into()));
        }
        let instance_id = InstanceId::from_str(&params.instance_id)?;
        let _claiming = self.inner.herdr_claims.lock.lock().await;
        if let Ok(existing) = self.inner.store.get_instance(&instance_id) {
            if existing.driver == DriverKind::HerdrClaimed
                && !matches!(
                    existing.lifecycle,
                    InstanceLifecycle::Exited | InstanceLifecycle::Failed
                )
            {
                return Ok(existing);
            }
            return Err(NodeError::Conflict(format!(
                "{HERDR_CLAIM_CONFLICT}: instance id {} is already in use",
                instance_id.as_id()
            )));
        }
        let resolution = self
            .herdr_resolve(&params.target, Some(&instance_id))
            .await?;
        let mut instance = fixture_instance_with_session(
            instance_id.clone(),
            self.host().meta.id,
            self.workspace_for_cwd(resolution.cwd.as_deref()),
            DriverKind::HerdrClaimed,
            None,
        )?;
        instance.kind = resolution.kind;
        instance.native_ref.kind = resolution.kind;
        instance.native_ref.claude = None;
        instance.native_ref.session_id = unknown("herdr-claim-has-no-native-session");
        instance.ownership = Ownership::AdoptedControl;
        instance.launched_by = Some(LaunchedBy::User);
        instance.lifecycle = InstanceLifecycle::Ready;
        if let Some(activity) = status_activity(&resolution.agent_status) {
            instance.activity = Knowledge::Known { value: activity };
        }
        instance.herdr_claim = Some(resolution.claim.clone());
        let store = self.inner.store.as_ref();
        store.insert_instance(instance)?;
        append_instance_lifecycle(
            store,
            &instance_id,
            Some("requested"),
            "ready",
            "herdr-claimed",
        )?;
        journal_status(
            store,
            &instance_id,
            &resolution.claim.pane_id,
            &resolution.agent_status,
        );
        tracing::info!(
            instance = %instance_id.as_id(),
            session = %resolution.claim.session,
            workspace = %resolution.claim.workspace_id,
            tab = %resolution.claim.tab_id,
            pane = %resolution.claim.pane_id,
            kind = ?resolution.kind,
            "herdr pane claimed"
        );
        self.start_claimed(&instance_id, resolution.claim, resolution.kind)
            .await;
        self.inner.store.get_instance(&instance_id)
    }

    /// Re-attach live claims after a Node start, before any command is
    /// served. Pane gone / process gone ends the claim with evidence; an
    /// unreachable server re-attaches in the contact-lost state. Never touches
    /// a pane.
    pub async fn reconcile_herdr_claims(&self) -> Result<(), NodeError> {
        for instance in self.inner.store.list_instances()? {
            if instance.driver != DriverKind::HerdrClaimed
                || matches!(
                    instance.lifecycle,
                    InstanceLifecycle::Exited | InstanceLifecycle::Failed
                )
                || self
                    .inner
                    .instance_drivers
                    .read()
                    .await
                    .contains_key(&instance.meta.id)
            {
                continue;
            }
            let id = instance.meta.id.clone();
            let Some(claim) = instance.herdr_claim.clone() else {
                end_claim(
                    self.inner.store.as_ref(),
                    &id,
                    "",
                    REASON_PANE_GONE,
                    "the claim record is missing; nothing to re-attach",
                );
                continue;
            };
            let client = Client::connect(&claim.socket_path).with_timeout(RPC_TIMEOUT);
            match probe(&client, &claim).await {
                Probe::Gone { reason, evidence } => {
                    tracing::info!(instance = %id.as_id(), reason, "claimed herdr pane ended while the Node was down");
                    end_claim(
                        self.inner.store.as_ref(),
                        &id,
                        &claim.pane_id,
                        reason,
                        &evidence,
                    );
                }
                Probe::Alive { .. } | Probe::Unreachable(_) => {
                    tracing::info!(instance = %id.as_id(), pane = %claim.pane_id, "re-attaching claimed herdr pane");
                    self.start_claimed(&id, claim, instance.kind).await;
                }
            }
        }
        Ok(())
    }

    /// Stop watching a claim for a Node shutdown without settling it: the next
    /// Node start re-attaches it. Never touches the pane.
    pub(crate) async fn suspend_claim(&self, id: &InstanceId) {
        let pane = self
            .inner
            .herdr_claims
            .live
            .lock()
            .ok()
            .and_then(|mut live| live.remove(id));
        if let Some(pane) = pane {
            pane.stop_watcher();
        }
        self.tty().stop(id).await;
    }

    async fn start_claimed(&self, id: &InstanceId, claim: HerdrClaim, kind: AgentKind) {
        let poll = self
            .inner
            .herdr_claims
            .poll
            .read()
            .map(|poll| *poll)
            .unwrap_or(DEFAULT_POLL);
        let binary = self
            .inner
            .herdr_config
            .as_ref()
            .and_then(|config| config.herdr_binary.clone());
        let pane = Arc::new(ClaimedPane {
            instance_id: id.clone(),
            socket: PathBuf::from(&claim.socket_path),
            claim,
            kind,
            binary,
            store: Arc::clone(&self.inner.store),
            tty: self.tty().clone(),
            poll,
            detached: AtomicBool::new(false),
            gone: AtomicBool::new(false),
            contact_lost: AtomicBool::new(false),
            watcher: std::sync::Mutex::new(None),
        });
        let handle = pane.spawn_watcher();
        // Registered with the observation pumps so a Node drop or shutdown
        // aborts it: a watcher outliving its Node would keep writing to a store
        // a successor reopened.
        self.inner.pumps.lock().await.insert(id.clone(), handle);
        if let Ok(mut live) = self.inner.herdr_claims.live.lock() {
            live.insert(id.clone(), Arc::clone(&pane));
        }
        // A herdr control session sizes the pane it attaches to. Open it at
        // the pane's current size so claiming never reflows the human's
        // terminal; a PWA viewer resizes it like any herdr client later.
        let (cols, rows) = pane_geometry(&pane.client(), &pane.claim.pane_id).await;
        if let Err(error) = self
            .tty()
            .start(id.clone(), pane.bridge(), cols, rows)
            .await
        {
            tracing::warn!(%error, instance = %id.as_id(), "claimed pane tty bridge failed to start");
        }
        self.adopt_worker(id.clone(), pane).await;
    }

    fn herdr_root(&self) -> PathBuf {
        self.inner
            .herdr_claims
            .root
            .read()
            .ok()
            .and_then(|root| root.clone())
            .unwrap_or_else(remuda_herdr::herdr_config_dir)
    }

    fn own_socket(&self) -> Option<PathBuf> {
        self.inner
            .herdr_config
            .as_ref()
            .map(crate::carrier_recovery::herdr_socket)
    }

    fn is_own_socket(&self, socket: &Path) -> bool {
        self.own_socket()
            .is_some_and(|own| canonical_string(&own) == canonical_string(socket))
    }

    fn resolve_session(&self, raw: &str) -> Result<SessionAddr, NodeError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(invalid("a herdr session name or socket path is required"));
        }
        let socket = if raw.starts_with('/') {
            PathBuf::from(raw)
        } else if !valid_session_name(raw) {
            return Err(invalid(format!(
                "herdr session {raw:?} is not a session name (letters, digits, '.', '_', '-') or an absolute socket path"
            )));
        } else if raw == "default" {
            self.herdr_root().join("herdr.sock")
        } else {
            self.herdr_root()
                .join("sessions")
                .join(raw)
                .join("herdr.sock")
        };
        if self.is_own_socket(&socket) {
            return Err(NodeError::InvalidRequest(format!(
                "{HERDR_CLAIM_OWN_SESSION}: {raw} is this Node's own carrier session; its panes already are Remuda instances"
            )));
        }
        Ok(SessionAddr {
            session: raw.to_owned(),
            socket,
        })
    }

    fn scan_sessions(&self) -> Vec<SessionAddr> {
        let root = self.herdr_root();
        let mut found = Vec::new();
        let default = root.join("herdr.sock");
        if default.exists() && !self.is_own_socket(&default) {
            found.push(SessionAddr {
                session: "default".to_owned(),
                socket: default,
            });
        }
        let mut names: Vec<String> = std::fs::read_dir(root.join("sessions"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| valid_session_name(name) && name != "default")
            .collect();
        names.sort();
        for name in names {
            let socket = root.join("sessions").join(&name).join("herdr.sock");
            if socket.exists() && !self.is_own_socket(&socket) {
                found.push(SessionAddr {
                    session: name,
                    socket,
                });
            }
        }
        found.truncate(MAX_SESSIONS);
        found
    }

    /// `(socket, pane) → instance` for every live claim on this Node.
    fn live_claims(&self) -> Result<Vec<(String, String, InstanceId)>, NodeError> {
        Ok(self
            .inner
            .store
            .list_instances()?
            .into_iter()
            .filter(|instance| {
                instance.driver == DriverKind::HerdrClaimed
                    && !matches!(
                        instance.lifecycle,
                        InstanceLifecycle::Exited | InstanceLifecycle::Failed
                    )
            })
            .filter_map(|instance| {
                let claim = instance.herdr_claim?;
                Some((claim.socket_path, claim.pane_id, instance.meta.id))
            })
            .collect())
    }

    fn live_claim_of(
        &self,
        socket: &str,
        pane_id: &str,
        except: Option<&InstanceId>,
    ) -> Result<Option<InstanceId>, NodeError> {
        Ok(self
            .live_claims()?
            .into_iter()
            .find(|(claim_socket, claim_pane, id)| {
                claim_socket == socket && claim_pane == pane_id && Some(id) != except
            })
            .map(|(_, _, id)| id))
    }

    /// The registered workspace holding the pane's cwd, else a fresh id that
    /// occupies no registered workspace.
    fn workspace_for_cwd(&self, cwd: Option<&str>) -> WorkspaceId {
        let Some(cwd) = cwd.map(Path::new).filter(|cwd| cwd.is_absolute()) else {
            return WorkspaceId::new();
        };
        self.workspaces()
            .ok()
            .and_then(|workspaces| {
                workspaces
                    .into_iter()
                    .find(|workspace| cwd.starts_with(&workspace.root_path))
            })
            .map_or_else(WorkspaceId::new, |workspace| workspace.meta.id)
    }
}

/// The driver behind a claimed pane: types into it, bridges its TTY, reads its
/// screen, and detaches on close. Never closes, kills or restarts anything.
pub(crate) struct ClaimedPane {
    instance_id: InstanceId,
    claim: HerdrClaim,
    kind: AgentKind,
    socket: PathBuf,
    binary: Option<PathBuf>,
    store: Arc<dyn LocalStore>,
    tty: TtyRegistry,
    poll: Duration,
    detached: AtomicBool,
    gone: AtomicBool,
    contact_lost: AtomicBool,
    watcher: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl ClaimedPane {
    fn client(&self) -> Client {
        Client::connect(&self.socket).with_timeout(RPC_TIMEOUT)
    }

    /// The control bridge: always the recorded socket and pane, never
    /// `--takeover`.
    fn bridge(&self) -> remuda_driver::TtyBridge {
        let mut client = Client::connect(&self.socket);
        if let Some(binary) = &self.binary {
            client = client.with_binary(binary);
        }
        remuda_driver::TtyBridge::Herdr {
            client,
            pane_id: self.claim.pane_id.clone(),
        }
    }

    fn spawn_watcher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let pane = Arc::clone(self);
        let handle = tokio::spawn(async move { pane.watch().await });
        if let Ok(mut slot) = self.watcher.lock() {
            *slot = Some(handle.abort_handle());
        }
        handle
    }

    fn stop_watcher(&self) {
        if let Some(handle) = self.watcher.lock().ok().and_then(|mut slot| slot.take()) {
            handle.abort();
        }
    }

    async fn watch(&self) {
        let mut last_status: Option<AgentStatus> = None;
        loop {
            match probe(&self.client(), &self.claim).await {
                Probe::Unreachable(error) => {
                    if !self.contact_lost.swap(true, Ordering::SeqCst) {
                        tracing::warn!(instance = %self.instance_id.as_id(), "claimed herdr server unreachable; contact lost");
                        journal_contact(
                            self.store.as_ref(),
                            &self.instance_id,
                            &self.claim,
                            "lost",
                            &error,
                        );
                    }
                }
                Probe::Gone { reason, evidence } => {
                    if self.detached.load(Ordering::SeqCst) {
                        return;
                    }
                    self.gone.store(true, Ordering::SeqCst);
                    tracing::info!(instance = %self.instance_id.as_id(), reason, "claimed herdr pane ended");
                    end_claim(
                        self.store.as_ref(),
                        &self.instance_id,
                        &self.claim.pane_id,
                        reason,
                        &evidence,
                    );
                    self.tty.stop(&self.instance_id).await;
                    return;
                }
                Probe::Alive { status } => {
                    if self.contact_lost.swap(false, Ordering::SeqCst) {
                        tracing::info!(instance = %self.instance_id.as_id(), "claimed herdr server answers again");
                        journal_contact(
                            self.store.as_ref(),
                            &self.instance_id,
                            &self.claim,
                            "ok",
                            "",
                        );
                    }
                    if last_status.is_some_and(|last| last != status) {
                        journal_status(
                            self.store.as_ref(),
                            &self.instance_id,
                            &self.claim.pane_id,
                            status_word(status),
                        );
                    }
                    last_status = Some(status);
                }
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    fn ensure_attached(&self) -> Result<(), DriverError> {
        if self.detached.load(Ordering::SeqCst) || self.gone.load(Ordering::SeqCst) {
            return Err(DriverError::ControlUnavailable);
        }
        Ok(())
    }

    /// herdr's agent row for the pane, when it detects one.
    async fn agent_status(&self) -> Option<AgentStatus> {
        let agent = self.client().agent_get(&self.claim.pane_id).await.ok()?;
        agent.agent.agent.as_ref()?;
        Some(agent.agent.agent_status)
    }

    async fn send_prompt(&self, prompt: String) -> Result<(), DriverError> {
        let client = self.client();
        if self.agent_status().await.is_some() {
            client
                .agent_prompt(AgentPromptParams {
                    target: self.claim.pane_id.clone(),
                    text: prompt,
                    wait: None,
                })
                .await
                .map_err(|error| pane_error("agent.prompt", &error))?;
            return Ok(());
        }
        client
            .pane_send_text(&self.claim.pane_id, prompt)
            .await
            .map_err(|error| pane_error("pane.send_text", &error))?;
        client
            .pane_send_keys(&self.claim.pane_id, vec!["enter".into()])
            .await
            .map_err(|error| pane_error("pane.send_keys", &error))?;
        Ok(())
    }

    async fn send_keys(&self, keys: Vec<String>) -> Result<(), DriverError> {
        self.client()
            .pane_send_keys(&self.claim.pane_id, keys)
            .await
            .map_err(|error| pane_error("pane.send_keys", &error))?;
        Ok(())
    }

    /// Detach: stop watching and bridging. The pane, its process and its
    /// server are left exactly as they are.
    async fn detach(&self) {
        self.detached.store(true, Ordering::SeqCst);
        self.stop_watcher();
        self.tty.stop(&self.instance_id).await;
        tracing::info!(instance = %self.instance_id.as_id(), pane = %self.claim.pane_id, "claimed herdr pane released (detached)");
    }

    async fn screen(&self) -> Result<Option<remuda_driver::ScreenRead>, DriverError> {
        self.ensure_attached()?;
        let read = self
            .client()
            .pane_read(PaneReadParams {
                pane_id: self.claim.pane_id.clone(),
                source: ReadSource::Visible,
                lines: None,
                format: ReadFormat::Text,
                strip_ansi: true,
            })
            .await
            .map_err(|error| pane_error("pane.read", &error))?;
        let mut lines: Vec<String> = read.text().lines().map(str::to_owned).collect();
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        let cols = lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        Ok(Some(remuda_driver::ScreenRead {
            cols: u16::try_from(cols).unwrap_or(u16::MAX),
            rows: u16::try_from(lines.len()).unwrap_or(u16::MAX),
            lines,
            cursor: (0, 0),
            alt_screen: false,
            // herdr's own terminal emulator produced the visible grid.
            emulated: true,
        }))
    }
}

impl Driver for ClaimedPane {
    fn kind(&self) -> DriverKind {
        DriverKind::HerdrClaimed
    }

    /// Agent panes take a prompt only while herdr reports the agent idle; the
    /// PTY queue retries until then. A plain shell pane is always ready.
    fn wait_control(&self) -> DriverFuture<'_> {
        Box::pin(async move {
            self.ensure_attached()?;
            if self.contact_lost.load(Ordering::SeqCst) {
                return Err(DriverError::ControlUnavailable);
            }
            match self.agent_status().await {
                Some(AgentStatus::Working | AgentStatus::Blocked) => {
                    Err(DriverError::ControlUnavailable)
                }
                _ => Ok(Vec::new()),
            }
        })
    }

    fn execute(&self, request: DriverRequest) -> DriverFuture<'_> {
        Box::pin(async move {
            match request {
                DriverRequest::Close => {
                    self.detach().await;
                }
                DriverRequest::Send {
                    prompt,
                    attachments,
                    ..
                } => {
                    self.ensure_attached()?;
                    if !attachments.is_empty() {
                        return Err(DriverError::Unsupported(
                            "attachments are not delivered to a claimed herdr pane".into(),
                        ));
                    }
                    self.send_prompt(prompt).await?;
                }
                DriverRequest::SendKeys { keys } => {
                    self.ensure_attached()?;
                    self.send_keys(keys).await?;
                }
                DriverRequest::Cancel => {
                    self.ensure_attached()?;
                    let key = if self.kind == AgentKind::Terminal {
                        "ctrl+c"
                    } else {
                        "esc"
                    };
                    self.send_keys(vec![key.into()]).await?;
                }
                DriverRequest::RespondInteraction { .. } => {
                    return Err(DriverError::Unsupported(
                        "a claimed herdr pane has no Remuda interactions; answer it on its screen"
                            .into(),
                    ));
                }
                DriverRequest::Configure { .. } => {
                    return Err(DriverError::Unsupported(
                        "a claimed herdr pane keeps the model and permission mode its owner set"
                            .into(),
                    ));
                }
            }
            Ok(Vec::new())
        })
    }

    fn tty_bridge(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<remuda_driver::TtyBridge>> + Send + '_>> {
        Box::pin(async move { self.ensure_attached().ok().map(|()| self.bridge()) })
    }

    fn process_gone(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move { self.gone.load(Ordering::SeqCst) })
    }

    fn screen_read(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<remuda_driver::ScreenRead>, DriverError>> + Send + '_,
        >,
    > {
        Box::pin(async move { self.screen().await })
    }
}

async fn list_session(
    addr: &SessionAddr,
    claimed: &[(String, String, InstanceId)],
) -> HerdrSessionListing {
    let client = Client::connect(&addr.socket).with_timeout(RPC_TIMEOUT);
    let socket_path = canonical_string(&addr.socket);
    let snapshot = match client.session_snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return HerdrSessionListing {
                session: addr.session.clone(),
                socket_path,
                reachable: false,
                error: Some(short_error(&error)),
                workspaces: Vec::new(),
            };
        }
    };
    let mut budget = MAX_PANES;
    let mut workspaces = snapshot.workspaces.clone();
    workspaces.sort_by(|a, b| (a.number, &a.workspace_id).cmp(&(b.number, &b.workspace_id)));
    let mut listed = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let mut tabs: Vec<_> = snapshot
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace.workspace_id)
            .collect();
        tabs.sort_by(|a, b| (a.number, &a.tab_id).cmp(&(b.number, &b.tab_id)));
        let mut tab_rows = Vec::with_capacity(tabs.len());
        for tab in tabs {
            let mut panes: Vec<_> = snapshot
                .panes
                .iter()
                .filter(|pane| {
                    pane.tab_id == tab.tab_id && pane.workspace_id == workspace.workspace_id
                })
                .collect();
            panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
            let mut pane_rows = Vec::with_capacity(panes.len());
            for pane in panes {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                pane_rows.push(pane_listing(&client, &snapshot, pane, &socket_path, claimed).await);
            }
            tab_rows.push(HerdrTabListing {
                tab_id: tab.tab_id.clone(),
                label: tab.label.clone(),
                panes: pane_rows,
            });
        }
        listed.push(HerdrWorkspaceListing {
            workspace_id: workspace.workspace_id.clone(),
            label: workspace.label.clone(),
            tabs: tab_rows,
        });
    }
    HerdrSessionListing {
        session: addr.session.clone(),
        socket_path,
        reachable: true,
        error: None,
        workspaces: listed,
    }
}

async fn pane_listing(
    client: &Client,
    snapshot: &SessionSnapshot,
    pane: &remuda_herdr::PaneInfo,
    socket_path: &str,
    claimed: &[(String, String, InstanceId)],
) -> HerdrPaneListing {
    let agent_row = snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane.pane_id);
    let agent = pane
        .agent
        .clone()
        .or_else(|| agent_row.and_then(|row| row.agent.clone()));
    let status = agent_row.map_or(pane.agent_status, |row| row.agent_status);
    let process = client
        .pane_process_info(Some(pane.pane_id.clone()))
        .await
        .ok()
        .and_then(|result| result.process_info)
        .and_then(|info| claim_process(&info, AgentKind::Generic));
    HerdrPaneListing {
        pane_id: pane.pane_id.clone(),
        agent,
        agent_status: status_word(status).to_owned(),
        process,
        claimed_by: claimed
            .iter()
            .find(|(socket, claimed_pane, _)| {
                socket == socket_path && *claimed_pane == pane.pane_id
            })
            .map(|(_, _, id)| id.as_id().to_string()),
    }
}

/// The pane's current size: rows from herdr's viewport, columns from the
/// widest visible line (agent TUIs draw full-width rules). Falls back to the
/// Node's TTY defaults for anything herdr does not say.
async fn pane_geometry(client: &Client, pane_id: &str) -> (u16, u16) {
    let rows = client
        .session_snapshot()
        .await
        .ok()
        .and_then(|snapshot| {
            snapshot
                .panes
                .into_iter()
                .find(|pane| pane.pane_id == pane_id)
        })
        .and_then(|pane| pane.scroll)
        .map(|scroll| scroll.viewport_rows)
        .filter(|rows| (2..=500).contains(rows))
        .and_then(|rows| u16::try_from(rows).ok())
        .unwrap_or(crate::TTY_DEFAULT_ROWS);
    let cols = client
        .pane_read(PaneReadParams {
            pane_id: pane_id.to_owned(),
            source: ReadSource::Visible,
            lines: None,
            format: ReadFormat::Text,
            strip_ansi: true,
        })
        .await
        .ok()
        .and_then(|read| read.text().lines().map(|line| line.chars().count()).max())
        .filter(|cols| *cols >= usize::from(crate::TTY_DEFAULT_COLS) && *cols <= 1000)
        .and_then(|cols| u16::try_from(cols).ok())
        .unwrap_or(crate::TTY_DEFAULT_COLS);
    (cols, rows)
}

/// Probe a claim: pane present in its recorded workspace and tab, process
/// alive, current agent status.
async fn probe(client: &Client, claim: &HerdrClaim) -> Probe {
    let snapshot = match client.session_snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => return Probe::Unreachable(short_error(&error)),
    };
    let Some(pane) = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_id == claim.pane_id)
    else {
        return Probe::Gone {
            reason: REASON_PANE_GONE,
            evidence: format!(
                "pane {} is no longer in herdr session {}",
                claim.pane_id, claim.session
            ),
        };
    };
    if pane.tab_id != claim.tab_id || pane.workspace_id != claim.workspace_id {
        return Probe::Gone {
            reason: REASON_PANE_GONE,
            evidence: format!(
                "pane id {} now belongs to tab {} in workspace {}, not the claimed tab {}",
                claim.pane_id, pane.tab_id, pane.workspace_id, claim.tab_id
            ),
        };
    }
    let status = snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == claim.pane_id)
        .map_or(pane.agent_status, |agent| agent.agent_status);
    if let Some(process) = &claim.process
        && let Ok(result) = client.pane_process_info(Some(claim.pane_id.clone())).await
        && let Some(info) = result.process_info
    {
        let listed = info.shell_pid == Some(process.pid)
            || info
                .foreground_processes
                .iter()
                .any(|candidate| candidate.pid == process.pid);
        // Both witnesses are required: an agent that hands the terminal to an
        // editor or a child drops out of the foreground list while alive.
        if !listed
            && kernel_alive(process.pid, process.start_ticks.map(|ticks| ticks.0)) == Some(false)
        {
            return Probe::Gone {
                reason: REASON_PROCESS_GONE,
                evidence: format!(
                    "process {} (pid {}) in pane {} has exited",
                    process.command, process.pid, claim.pane_id
                ),
            };
        }
    }
    Probe::Alive { status }
}

/// Journal process-end evidence and settle the instance `exited`. The pane is
/// never touched.
fn end_claim(store: &dyn LocalStore, id: &InstanceId, pane_id: &str, reason: &str, evidence: &str) {
    let Ok(instance) = store.get_instance(id) else {
        return;
    };
    if matches!(
        instance.lifecycle,
        InstanceLifecycle::Exited | InstanceLifecycle::Failed
    ) {
        return;
    }
    journal_native(
        store,
        id,
        LifecycleTopic::Session,
        "exit",
        pane_id,
        "exited",
        BTreeMap::from([
            ("reasonCode".to_owned(), reason.to_owned()),
            ("lastError".to_owned(), reason.to_owned()),
            ("evidence".to_owned(), evidence.to_owned()),
        ]),
        Severity::Info,
    );
    if let Err(error) = store.mark_unsettled_unknown(id) {
        tracing::warn!(%error, "unsettled commands not marked before a claim ended");
    }
    if let Err(error) = append_instance_lifecycle(store, id, Some("ready"), "exited", reason) {
        tracing::error!(%error, "claim end lifecycle not appended");
    }
    if let Err(error) = store.set_instance_state(
        id,
        Some(InstanceLifecycle::Exited),
        Some(Knowledge::Known {
            value: Activity::Idle,
        }),
    ) {
        tracing::error!(%error, "claimed instance not marked exited");
    }
}

fn journal_status(store: &dyn LocalStore, id: &InstanceId, pane_id: &str, status: &str) {
    let word = if status == "done" { "idle" } else { status };
    journal_native(
        store,
        id,
        LifecycleTopic::Turn,
        "agent_status",
        pane_id,
        word,
        BTreeMap::new(),
        Severity::Info,
    );
    if let Some(activity) = status_activity(word)
        && let Err(error) =
            store.set_instance_state(id, None, Some(Knowledge::Known { value: activity }))
    {
        tracing::warn!(%error, "claimed instance activity not updated");
    }
}

fn journal_contact(
    store: &dyn LocalStore,
    id: &InstanceId,
    claim: &HerdrClaim,
    state: &str,
    error: &str,
) {
    let mut related = BTreeMap::from([("session".to_owned(), claim.session.clone())]);
    if !error.is_empty() {
        related.insert("error".to_owned(), error.to_owned());
    }
    journal_native(
        store,
        id,
        LifecycleTopic::Diagnostic,
        CONTACT_EVENT,
        &claim.pane_id,
        state,
        related,
        if state == "lost" {
            Severity::Warning
        } else {
            Severity::Info
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn journal_native(
    store: &dyn LocalStore,
    id: &InstanceId,
    topic: LifecycleTopic,
    name: &str,
    pane_id: &str,
    status: &str,
    related_ids: BTreeMap<String, String>,
    severity: Severity,
) {
    let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
        NativeLifecycle {
            topic,
            native_name: name.to_owned(),
            native_id: Knowledge::Known {
                value: pane_id.to_owned(),
            },
            status: Knowledge::Known {
                value: status.to_owned(),
            },
            related_ids,
            data_ref: None,
            severity,
            affects_completion: false,
        },
    ))));
    if let Err(error) = store.append_observation(id, None, Completeness::Structured, payload) {
        tracing::warn!(%error, name, "claim journal event not appended");
    }
}

fn claim_kind(explicit: Option<&str>, agent: Option<&str>) -> Result<AgentKind, NodeError> {
    if let Some(kind) = explicit.map(str::trim).filter(|kind| !kind.is_empty()) {
        return match kind.to_ascii_lowercase().as_str() {
            "claude" => Ok(AgentKind::Claude),
            "codex" => Ok(AgentKind::Codex),
            "grok" => Ok(AgentKind::Grok),
            "agy" => Ok(AgentKind::Agy),
            "shell" | "terminal" => Ok(AgentKind::Terminal),
            "generic" => Ok(AgentKind::Generic),
            other => Err(invalid(format!(
                "kind {other:?} is not one of claude, codex, grok, agy, shell"
            ))),
        };
    }
    Ok(match agent.map(str::to_ascii_lowercase).as_deref() {
        Some("claude") => AgentKind::Claude,
        Some("codex") => AgentKind::Codex,
        Some("grok") => AgentKind::Grok,
        Some("agy") => AgentKind::Agy,
        Some(_) => AgentKind::Generic,
        None => AgentKind::Terminal,
    })
}

/// The process a claim watches: the pane's shell for a `terminal` claim, else
/// its foreground process-group leader.
fn claim_process(info: &PaneProcessInfo, kind: AgentKind) -> Option<HerdrClaimProcess> {
    let named = |pid: u32| {
        info.foreground_processes
            .iter()
            .find(|process| process.pid == pid)
            .map(|process| base_name(process.argv0.as_deref().unwrap_or(&process.name)))
            .or_else(|| kernel_comm(pid))
            .unwrap_or_else(|| "shell".to_owned())
    };
    if kind == AgentKind::Terminal
        && let Some(pid) = info.shell_pid
    {
        return Some(HerdrClaimProcess {
            pid,
            command: named(pid),
            start_ticks: None,
        });
    }
    let leader = info
        .foreground_processes
        .iter()
        .find(|process| Some(process.pid) == info.foreground_process_group_id)
        .or_else(|| info.foreground_processes.first());
    match leader {
        Some(process) => Some(HerdrClaimProcess {
            pid: process.pid,
            command: base_name(process.argv0.as_deref().unwrap_or(&process.name)),
            start_ticks: None,
        }),
        None => info.shell_pid.map(|pid| HerdrClaimProcess {
            pid,
            command: named(pid),
            start_ticks: None,
        }),
    }
}

/// Executable base name only; argv is never collected (it may carry secrets).
fn base_name(raw: &str) -> String {
    let name = Path::new(raw.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(raw)
        .trim_start_matches('-');
    name.chars().take(64).collect()
}

fn status_word(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Unknown => "unknown",
    }
}

fn status_activity(word: &str) -> Option<Activity> {
    match word {
        "idle" | "done" => Some(Activity::Idle),
        "working" => Some(Activity::Working),
        "blocked" => Some(Activity::WaitingInteraction),
        _ => None,
    }
}

fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn canonical_string(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn invalid(detail: impl std::fmt::Display) -> NodeError {
    NodeError::InvalidRequest(format!("{HERDR_CLAIM_INVALID}: {detail}"))
}

fn unreachable_error(addr: &SessionAddr, error: &remuda_herdr::Error) -> NodeError {
    NodeError::InvalidRequest(format!(
        "{HERDR_UNREACHABLE}: herdr session {} did not answer: {}",
        addr.session,
        short_error(error)
    ))
}

/// A claimed pane's failure, worded so the Node's own carrier recovery never
/// mistakes it for loss of the Node's session (`carrier_recovery::is_carrier_loss`).
fn pane_error(call: &str, error: &remuda_herdr::Error) -> DriverError {
    let text = short_error(error)
        .replace(remuda_herdr::SERVER_UNAVAILABLE, "not answering")
        .replace("server is shutting down", "stopping")
        .replace("carrier unavailable", "unavailable")
        .replace("herdr unreachable", "unreachable");
    DriverError::Failed(format!("claimed pane {call} failed: {text}"))
}

fn short_error(error: &remuda_herdr::Error) -> String {
    error.to_string().chars().take(240).collect()
}

/// Kernel start time of `pid` (Linux `/proc/<pid>/stat` field 22).
fn kernel_start_ticks(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 2 (comm) may contain spaces; everything after its closing
        // paren starts at field 3, so field 22 is the 20th from there.
        let rest = stat.rsplit_once(')')?.1;
        rest.split_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// The kernel's answer for "is this the claimed process": `Some(false)` only
/// on positive evidence (no such pid, or a different start time).
fn kernel_alive(pid: u32, start_ticks: Option<u64>) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(_) => match (start_ticks, kernel_start_ticks(pid)) {
                (Some(recorded), Some(now)) => Some(recorded == now),
                _ => Some(true),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
            Err(_) => None,
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = start_ticks;
        let Ok(pid) = i32::try_from(pid) else {
            return None;
        };
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
            Ok(()) | Err(nix::errno::Errno::EPERM) => Some(true),
            Err(nix::errno::Errno::ESRCH) => Some(false),
            Err(_) => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, start_ticks);
        None
    }
}

/// `/proc/<pid>/comm`: the kernel's short process name.
fn kernel_comm(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|comm| base_name(comm.trim()))
            .filter(|comm| !comm.is_empty())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests;
