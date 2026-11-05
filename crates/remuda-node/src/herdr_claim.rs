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
    HERDR_CLAIM_AMBIGUOUS, HERDR_CLAIM_CONFLICT, HERDR_CLAIM_INVALID, HERDR_CLAIM_NOT_READY,
    HERDR_CLAIM_OWN_SESSION, HERDR_UNREACHABLE, HerdrClaimResolution, HerdrClaimTarget,
    HerdrDiscoverParams, HerdrDiscoverResult, HerdrPaneListing, HerdrSessionListing,
    HerdrTabListing, HerdrWorkspaceListing, InstanceClaimParams, METHOD_HERDR_DISCOVER,
    METHOD_HERDR_RESOLVE, METHOD_INSTANCE_CLAIM,
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
        atomic::{AtomicBool, AtomicU32, Ordering},
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
/// Reason code when herdr answers but cannot say what runs in the claimed
/// pane for [`UNKNOWN_PROBES_TO_END`] probes in a row, or the claim recorded
/// no process to compare with (D-062 r3 item 3): the claim fails closed.
pub const REASON_PROCESS_UNKNOWN: &str = "herdr-process-unknown";
/// Reason code of a release (detach; the pane keeps running).
pub const REASON_RELEASED: &str = "released";
/// Consecutive probes whose `pane.process_info` gives no answer (while the
/// session snapshot does) before the claim ends.
const UNKNOWN_PROBES_TO_END: u32 = 3;
/// Native diagnostic carrying `lost` / `ok` contact with the herdr server.
pub const CONTACT_EVENT: &str = "herdr_claim_contact";

/// Hub→Node methods that write to a pane or end its claim (D-062 r2).
const PANE_WRITE_METHODS: &[&str] = &[
    "instance.send",
    "instance.configure",
    "instance.cancel",
    "instance.respond",
    "interaction.respond",
    "tty.write",
    "instance.keys",
    "tty.resize",
    "instance.close",
    "instance.purge",
];

/// D-062 r2/r3, Node side of the Human-only rule: refuse a write to a
/// claimed pane unless its frame carries the Hub-stamped origin `human`. A
/// frame with NO origin is refused too (r3 item 4): every Hub that can create
/// a claimed instance stamps the origin of a pane write, so a missing one is
/// an omission and must never downgrade the check.
pub(crate) fn refuse_non_human_write(
    node: &DevNode,
    method: &str,
    params: &Value,
) -> Result<(), NodeError> {
    if !PANE_WRITE_METHODS.contains(&method) {
        return Ok(());
    }
    if params.get("origin").and_then(Value::as_str) == Some("human") {
        return Ok(());
    }
    let Some(id) = params
        .get("instanceId")
        .and_then(Value::as_str)
        .and_then(|id| InstanceId::from_str(id).ok())
    else {
        return Ok(());
    };
    match node.inner.store.get_instance(&id) {
        Ok(instance) if instance.driver == DriverKind::HerdrClaimed => {
            tracing::warn!(instance = %id.as_id(), method, "non-human write to a claimed herdr pane refused");
            Err(NodeError::InvalidRequest(format!(
                "human-only: {method} to a claimed herdr pane needs a human operator (D-062)"
            )))
        }
        _ => Ok(()),
    }
}

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
    /// The server answered but `pane.process_info` did not: nothing can be
    /// said about the claimed process, so it is never read as alive.
    Unknown(String),
    /// Pane present, the claimed process still in its role.
    Alive { status: AgentStatus },
}

/// The claimed process now, against the identity recorded at claim time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    /// Same pid and kernel start time, still in its role (agent leader or
    /// shell).
    Same,
    /// The claim is over: the process left its role, exited, or none was
    /// recorded to compare with.
    Ended {
        reason: &'static str,
        evidence: String,
    },
    /// `pane.process_info` failed or answered without process info.
    Unknown(String),
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
        let process = info.as_ref().and_then(|info| claimed_process(info, kind));
        if process.is_none() {
            return Err(NodeError::Conflict(format!(
                "{HERDR_CLAIM_NOT_READY}: pane {pane_id} runs no {} process yet (herdr reports only its shell); start it, then claim again",
                if kind == AgentKind::Terminal {
                    "shell"
                } else {
                    "agent"
                }
            )));
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
            remuda_workspace_id: self
                .registered_workspace_for_cwd(pane.cwd.as_deref())
                .map(|id| id.as_id().to_string()),
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
        self.start_claimed(&instance_id, resolution.claim, resolution.kind, false, 0)
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
            match probe(&client, &claim, instance.kind).await {
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
                probe @ (Probe::Alive { .. } | Probe::Unreachable(_) | Probe::Unknown(_)) => {
                    tracing::info!(instance = %id.as_id(), pane = %claim.pane_id, "re-attaching claimed herdr pane");
                    // D-062 r2 item 3: a contact state journaled before the
                    // restart is stale; journal what this Node sees now, so a
                    // `lost` that survived the restart flips back to `ok`.
                    let (lost, error) = match &probe {
                        Probe::Unreachable(error) => (true, error.clone()),
                        _ => (false, String::new()),
                    };
                    journal_contact(
                        self.inner.store.as_ref(),
                        &id,
                        &claim,
                        if lost { "lost" } else { "ok" },
                        &error,
                    );
                    // An unknown identity re-attaches with every write
                    // refused and counts toward the end like a live probe.
                    let unknown = u32::from(matches!(probe, Probe::Unknown(_)));
                    self.start_claimed(&id, claim, instance.kind, lost, unknown)
                        .await;
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

    async fn start_claimed(
        &self,
        id: &InstanceId,
        claim: HerdrClaim,
        kind: AgentKind,
        contact_lost: bool,
        unknown_probes: u32,
    ) {
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
            contact_lost: AtomicBool::new(contact_lost),
            unknown_probes: AtomicU32::new(unknown_probes),
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
        // D-062 r3 item 2: every raw input write re-checks the claimed
        // process first (the guard holds the pane weakly: no cycle through
        // the registry).
        let guard: Arc<dyn crate::TtyWriteGuard> = Arc::new(ClaimWriteGuard(Arc::downgrade(&pane)));
        if let Err(error) = self
            .tty()
            .start_guarded(id.clone(), pane.bridge(), cols, rows, Some(guard))
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
        self.registered_workspace_for_cwd(cwd).unwrap_or_default()
    }

    /// The registered workspace whose root contains the pane's cwd.
    fn registered_workspace_for_cwd(&self, cwd: Option<&str>) -> Option<WorkspaceId> {
        let cwd = cwd.map(Path::new).filter(|cwd| cwd.is_absolute())?;
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        self.workspaces()
            .ok()?
            .into_iter()
            .find(|workspace| cwd.starts_with(&workspace.root_path))
            .map(|workspace| workspace.meta.id)
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
    /// Consecutive probes with no process info (D-062 r3 item 3).
    unknown_probes: AtomicU32,
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
            match probe(&self.client(), &self.claim, self.kind).await {
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
                    self.end(reason, &evidence).await;
                    return;
                }
                Probe::Unknown(error) => {
                    let streak = self.unknown_probes.fetch_add(1, Ordering::SeqCst) + 1;
                    if streak == 1 {
                        tracing::warn!(instance = %self.instance_id.as_id(), pane = %self.claim.pane_id, "claimed pane's process is unknown; writes refused");
                    }
                    if streak >= UNKNOWN_PROBES_TO_END {
                        let evidence = format!(
                            "herdr answered but reported no process for pane {} in {streak} probes in a row ({error}); the claim cannot be verified",
                            self.claim.pane_id
                        );
                        self.end(REASON_PROCESS_UNKNOWN, &evidence).await;
                        return;
                    }
                }
                Probe::Alive { status } => {
                    self.unknown_probes.store(0, Ordering::SeqCst);
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

    /// D-062 r3 item 2: the gate in front of every write to the pane. It
    /// re-reads `pane.process_info` and the kernel start time NOW and compares
    /// them with the recorded identity, so a write in the window between two
    /// probes never reaches a process that replaced the claimed one. A
    /// replaced process ends the claim and the write is dropped; an unknown
    /// one refuses the write.
    async fn admit_write(&self) -> Result<(), DriverError> {
        self.ensure_attached()?;
        match identity(&self.client(), &self.claim, self.kind).await {
            Identity::Same => Ok(()),
            Identity::Ended { reason, evidence } => {
                self.end(reason, &evidence).await;
                Err(claim_failure(format!(
                    "claimed pane write dropped; the claim ended: {evidence}"
                )))
            }
            Identity::Unknown(error) => Err(claim_failure(format!(
                "claimed pane write refused: herdr cannot say what runs in pane {} ({error})",
                self.claim.pane_id
            ))),
        }
    }

    /// End the claim with process-end evidence: journal it, settle the row
    /// `exited`, stop the bridge and the watcher. Once; never after a
    /// release; never touches the pane.
    async fn end(&self, reason: &'static str, evidence: &str) {
        if self.detached.load(Ordering::SeqCst) || self.gone.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::info!(instance = %self.instance_id.as_id(), reason, "claimed herdr pane ended");
        end_claim(
            self.store.as_ref(),
            &self.instance_id,
            &self.claim.pane_id,
            reason,
            evidence,
        );
        self.tty.stop(&self.instance_id).await;
        // Last: when the watcher itself ends the claim this aborts its own
        // task, which then returns.
        self.stop_watcher();
    }

    async fn send_prompt(&self, prompt: String) -> Result<(), DriverError> {
        self.admit_write().await?;
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
        self.admit_write().await?;
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

    /// Never handed out: the claim's only bridge is the guarded one
    /// [`DevNode::start_claimed`] opens, whose every input write re-checks the
    /// claimed process. An unguarded bridge would skip that check.
    fn tty_bridge(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<remuda_driver::TtyBridge>> + Send + '_>> {
        Box::pin(std::future::ready(None))
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

/// The claimed bridge's [`crate::TtyWriteGuard`]: [`ClaimedPane::admit_write`]
/// before every raw input write.
struct ClaimWriteGuard(std::sync::Weak<ClaimedPane>);

impl crate::TtyWriteGuard for ClaimWriteGuard {
    fn admit(&self) -> Pin<Box<dyn Future<Output = Result<(), NodeError>> + Send + '_>> {
        Box::pin(async move {
            let Some(pane) = self.0.upgrade() else {
                return Err(NodeError::DriverUnavailable);
            };
            pane.admit_write()
                .await
                .map_err(|error| NodeError::Driver(error.to_string()))
        })
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
        .and_then(|info| listing_process(&info));
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
async fn probe(client: &Client, claim: &HerdrClaim, kind: AgentKind) -> Probe {
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
    match identity(client, claim, kind).await {
        Identity::Same => Probe::Alive { status },
        Identity::Ended { reason, evidence } => Probe::Gone { reason, evidence },
        Identity::Unknown(error) => Probe::Unknown(error),
    }
}

/// D-062 r2/r3: compare the process now in the claimed role (agent leader or
/// shell) with the recorded pid and kernel start time. Used by every probe
/// and before every write.
async fn identity(client: &Client, claim: &HerdrClaim, kind: AgentKind) -> Identity {
    let Some(process) = &claim.process else {
        // Round-1 data: nothing to compare with, so nothing is ever verified.
        return Identity::Ended {
            reason: REASON_PROCESS_UNKNOWN,
            evidence: format!(
                "the claim on pane {} recorded no process identity; claim the pane again",
                claim.pane_id
            ),
        };
    };
    let info = match client.pane_process_info(Some(claim.pane_id.clone())).await {
        Ok(result) => match result.process_info {
            Some(info) => info,
            None => {
                return Identity::Unknown(format!(
                    "pane.process_info reported nothing for pane {}",
                    claim.pane_id
                ));
            }
        },
        Err(error) => {
            return Identity::Unknown(format!("pane.process_info failed: {}", short_error(&error)));
        }
    };
    let now = current_claimed_pid(&info, kind);
    let ticks = now.and_then(kernel_start_ticks);
    if same_process(process, now, ticks) {
        return Identity::Same;
    }
    let role = if kind == AgentKind::Terminal {
        "shell"
    } else {
        "foreground agent"
    };
    let evidence = match now {
        Some(pid) if pid == process.pid && ticks.is_none() => format!(
            "process {} (pid {}) in pane {} has exited",
            process.command, process.pid, claim.pane_id
        ),
        Some(pid) if pid == process.pid => format!(
            "pid {} in pane {} was reused by another process (start time changed)",
            process.pid, claim.pane_id
        ),
        Some(pid) => format!(
            "the claimed {} (pid {}) is no longer the {role} of pane {}: pid {pid} is",
            process.command, process.pid, claim.pane_id
        ),
        None => format!(
            "the claimed {} (pid {}) left pane {}; it has no {role} now",
            process.command, process.pid, claim.pane_id
        ),
    };
    Identity::Ended {
        reason: REASON_PROCESS_GONE,
        evidence,
    }
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
/// The pane's foreground process-group leader, if it is not the shell.
fn agent_leader(info: &PaneProcessInfo) -> Option<&remuda_herdr::PaneProcessInfoProcess> {
    let leader = info.foreground_process_group_id?;
    if Some(leader) == info.shell_pid {
        return None;
    }
    info.foreground_processes
        .iter()
        .find(|process| process.pid == leader)
}

fn process_name(info: &PaneProcessInfo, pid: u32) -> String {
    info.foreground_processes
        .iter()
        .find(|process| process.pid == pid)
        .map(|process| base_name(process.argv0.as_deref().unwrap_or(&process.name)))
        .or_else(|| kernel_comm(pid))
        .unwrap_or_else(|| "shell".to_owned())
}

/// What discovery shows for a pane: its agent leader, else its shell.
fn listing_process(info: &PaneProcessInfo) -> Option<HerdrClaimProcess> {
    let pid = agent_leader(info)
        .map(|process| process.pid)
        .or(info.shell_pid)?;
    Some(HerdrClaimProcess {
        pid,
        command: process_name(info, pid),
        start_ticks: None,
    })
}

/// D-062 r2: the process a claim watches, with its kernel start time.
///
/// * an agent kind claims the pane's foreground process-group leader, which
///   must not be the shell: with no such leader the pane runs no agent yet
///   and the claim is refused (`None`), never pinned to the shell;
/// * `terminal` (`--kind shell`) claims the pane's shell itself. A different
///   foreground job is allowed; the shell's own replacement ends the claim.
fn claimed_process(info: &PaneProcessInfo, kind: AgentKind) -> Option<HerdrClaimProcess> {
    let pid = if kind == AgentKind::Terminal {
        info.shell_pid?
    } else {
        agent_leader(info)?.pid
    };
    Some(HerdrClaimProcess {
        pid,
        command: process_name(info, pid),
        start_ticks: kernel_start_ticks(pid).map(U64),
    })
}

/// The process now in the claimed role (agent leader or shell).
fn current_claimed_pid(info: &PaneProcessInfo, kind: AgentKind) -> Option<u32> {
    if kind == AgentKind::Terminal {
        info.shell_pid
    } else {
        agent_leader(info).map(|process| process.pid)
    }
}

/// Whether `pid` is still the recorded process: same pid AND, when a start
/// time was recorded, the same kernel start time (a reused pid fails).
fn same_process(recorded: &HerdrClaimProcess, pid: Option<u32>, ticks_now: Option<u64>) -> bool {
    pid == Some(recorded.pid)
        && recorded
            .start_ticks
            .is_none_or(|ticks| ticks_now == Some(ticks.0))
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
    claim_failure(format!(
        "claimed pane {call} failed: {}",
        short_error(error)
    ))
}

/// A claim's driver failure, worded the same way as [`pane_error`].
fn claim_failure(text: String) -> DriverError {
    let text = text
        .replace(remuda_herdr::SERVER_UNAVAILABLE, "not answering")
        .replace("server is shutting down", "stopping")
        .replace("carrier unavailable", "unavailable")
        .replace("herdr unreachable", "unreachable");
    DriverError::Failed(text)
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
