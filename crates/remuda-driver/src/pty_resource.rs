//! Ownership and bounded reclamation of Herdr runtime resources (never native homes).

use crate::{DriverError, DriverResult};
use remuda_herdr::{
    Client, EventKind, Pong, SessionSnapshot, Subscription, WorkspaceCreateParams, WorkspaceCreated,
};
use remuda_protocol::InstanceId;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

/// How long a stopped agent may take to leave its pane before the pane is
/// closed anyway. The Instance is already gone from the Hub's point of view,
/// so waiting longer only leaks panes into the operator's Herdr session.
pub(crate) const AGENT_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Bounded wait for a forced pane close to be reflected by Herdr (the close
/// RPC itself removes the agent synchronously, so the first verification poll
/// normally confirms it).
const FORCE_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Verified outcome of the stop phase ([`PtyResource::interrupt`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterruptOutcome {
    /// No owned workspace was present: there was no native writer to stop.
    Absent,
    /// The native writer was observed to leave within the grace.
    ExitedGracefully,
    /// The grace elapsed (or status RPCs failed during shutdown) without an
    /// observed exit; the writer was force-terminated and its exit verified.
    ForceTerminated,
}

/// Herdr server-boot identity captured when a workspace is created.
///
/// Herdr allocates ids deterministically (`w1`, `w1:t1`, `w1:p1`), so a
/// restarted server hands the SAME ids to unrelated workspaces. The creation
/// label (on the workspace) is the primary guard; this facet is
/// defence-in-depth captured independently of the workspace set: a boot whose
/// `ping` identity no longer matches must never receive a destructive call
/// from a resource created against its predecessor (c-ctxusage r14 item 1).
/// `None` means the facet could not be captured (older persisted resource, or
/// `ping` unavailable at creation); the creation label then guards alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ServerFacet {
    /// Herdr version reported at creation.
    pub version: String,
    /// Protocol number reported at creation.
    pub protocol: u32,
    /// Endpoint generation reported in the `ping` capabilities at creation.
    #[serde(default)]
    pub endpoint_generation: Option<u32>,
}

impl ServerFacet {
    fn of(pong: &Pong) -> Self {
        Self {
            version: pong.version.clone(),
            protocol: pong.protocol,
            endpoint_generation: pong
                .capabilities
                .as_ref()
                .and_then(|caps| caps.endpoint_protocol_generation),
        }
    }
}

/// Who, RIGHT NOW, holds a deterministic Herdr id this resource was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ownership {
    /// The id is not live on the server: a close against it is a harmless
    /// no-op, not a foreign kill.
    Absent,
    /// The live id still carries this resource's creation label and boot.
    Ours,
    /// The live id belongs to a different workspace or a restarted server:
    /// every destructive call must be refused.
    Foreign,
}

/// A workspace created exclusively for one Node instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyResource {
    /// Node identity, absent for standalone driver callers.
    pub instance_id: Option<InstanceId>,
    /// Exact API endpoint, independent of the caller's focused Herdr session.
    pub socket_path: PathBuf,
    /// Isolated session name.
    pub session: String,
    /// Workspace handle.
    pub workspace_id: String,
    /// Unique creation label protects against ID reuse after a server restart.
    pub workspace_label: String,
    /// Server-boot identity captured at creation; re-proven before every
    /// destructive close (c-ctxusage r14 item 1).
    #[serde(default)]
    pub server_facet: Option<ServerFacet>,
    /// Tab containing the agent and its root shell.
    pub tab_id: String,
    /// Agent pane (initially the root pane during materialization).
    pub pane_id: String,
}

impl PtyResource {
    /// Stable store key within an API endpoint.
    pub fn key(&self) -> String {
        format!("{}:{}", self.socket_path.display(), self.workspace_id)
    }

    /// A short-lived socket client; no implicit launch or default-session fallback.
    pub fn client(&self) -> Client {
        Client::connect(&self.socket_path).with_timeout(Duration::from_secs(2))
    }

    /// Phase 1 of close: interrupt the native writer (Ctrl-C) and wait a
    /// bounded grace for it to leave, while every pane, tab and the workspace
    /// stay alive.
    ///
    /// The collector drains the usage record the native appends WHILE HANDLING
    /// the interrupt (the interrupted turn) only if it is still reading when
    /// that append lands, so callers must run this BEFORE their final
    /// transcript drain and [`Self::reap`] only afterwards
    /// (c-ctxusage r12 item 1).
    ///
    /// Returns a VERIFIED outcome (c-ctxusage r13 item 1): when the writer has
    /// not left by the grace — or the status RPCs fail during Herdr's shutdown
    /// window — it is force-terminated (the agent pane is closed, reaping the
    /// foreground process group) and its exit is verified, still while every
    /// other pane/tab/workspace and the caller's collector stay alive. The
    /// drain that follows therefore never races a live writer's final append.
    pub(crate) async fn interrupt(&self, grace: Duration) -> DriverResult<InterruptOutcome> {
        let client = self.client();
        if !self.owned_workspace_present(&client).await? {
            return Ok(InterruptOutcome::Absent);
        }
        if self.await_polite_exit(&client, grace).await {
            return Ok(InterruptOutcome::ExitedGracefully);
        }
        self.force_terminate(&client).await?;
        Ok(InterruptOutcome::ForceTerminated)
    }

    /// Phase 2 of close: close and verify every pane, tab and the workspace
    /// this Instance owns, AFTER the caller drained the final transcript
    /// append.
    ///
    /// Success proves carrier removal, never task success.
    pub async fn reap(&self) -> DriverResult<()> {
        let client = self.client();
        if !self.owned_workspace_present(&client).await? {
            return Ok(());
        }
        // Every pane here belongs to this Instance: the workspace was created
        // for it with a unique label, and the check above proved this is still
        // that workspace and not an id reused after a server restart.
        for pane in self.owned_panes(&client).await? {
            // Re-validate ownership IMMEDIATELY before the destructive call:
            // the listing snapshot is already stale by the time the RPC goes
            // out, and a Herdr restart inside that window hands the
            // deterministic pane id to an unrelated workspace (c-ctxusage r14
            // item 1).
            self.refuse_foreign_pane(&client, &pane).await?;
            let _ = client.pane_close(&pane).await;
        }
        // The tab reclaims the root shell left behind by `pane.split`.
        self.refuse_foreign_tab(&client, &self.tab_id).await?;
        let _ = client.tab_close(&self.tab_id).await;
        if self.owned_workspace_present(&client).await? {
            let _ = client.workspace_close(&self.workspace_id).await;
        }
        if self.owned_workspace_present(&client).await? {
            return Err(DriverError::InvalidLaunchSpec(
                "owned Herdr workspace survived close".into(),
            ));
        }
        // Only count panes still held under OUR label: after a mid-reap
        // restart the id may belong to a foreign workspace, in which case the
        // refusal above (or the label checks) ends the close with an error
        // rather than reporting foreign panes as owned survivors.
        let (facet_ok, snapshot) = self.audit(&client).await?;
        let surviving: Vec<String> = snapshot
            .panes
            .iter()
            .filter(|pane| {
                pane.workspace_id == self.workspace_id
                    && matches!(
                        self.pane_ownership(&pane.pane_id, facet_ok, &snapshot),
                        Ownership::Ours
                    )
            })
            .map(|pane| pane.pane_id.clone())
            .collect();
        if !surviving.is_empty() {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "{} owned Herdr pane(s) survived close",
                surviving.len()
            )));
        }
        Ok(())
    }

    /// Interrupt the agent, then close and verify every pane, tab and the
    /// workspace this Instance owns. Equivalent to [`Self::interrupt`]
    /// followed by [`Self::reap`]; callers that must drain a native
    /// transcript append between the two phases call them separately.
    ///
    /// The interrupt is a courtesy with a bounded grace: an agent that ignores
    /// `ctrl+c` must not keep a pane alive, because the Instance is already
    /// gone as far as the Hub is concerned and the pane would be an orphan in
    /// the operator's Herdr session forever. After the grace the agent pane is
    /// closed explicitly — closing the tab alone is not enough, since
    /// `pane.split` may have landed the agent outside the recorded tab.
    ///
    /// Success proves carrier removal, never task success.
    pub async fn close(&self) -> DriverResult<()> {
        self.interrupt(AGENT_EXIT_GRACE).await?;
        self.reap().await
    }

    /// Is this exact workspace — same id, creation label AND server boot —
    /// still present?
    ///
    /// Herdr allocates ids deterministically (`w1`, `w1:t1`, `w1:p1`), so a
    /// restarted server hands the same ids to unrelated workspaces. The
    /// `remuda-<uuid>` creation label distinguishes ours; the captured server
    /// facet independently proves the boot did not change. Both stay
    /// preconditions of every destructive action: never close someone else's
    /// pane.
    async fn owned_workspace_present(&self, client: &Client) -> DriverResult<bool> {
        let (facet_ok, snapshot) = self.audit(client).await?;
        Ok(matches!(
            self.workspace_ownership(facet_ok, &snapshot),
            Ownership::Ours
        ))
    }

    /// One `ping` plus one `session.snapshot`, taken together immediately
    /// before a destructive decision. `facet_ok` is false when the live
    /// server's boot identity no longer matches the one captured at creation.
    async fn audit(&self, client: &Client) -> DriverResult<(bool, SessionSnapshot)> {
        let facet_ok = match &self.server_facet {
            None => true,
            Some(facet) => {
                let pong = client.ping().await.map_err(super::claude_pty::map_herdr)?;
                facet == &ServerFacet::of(&pong)
            }
        };
        let snapshot = client
            .session_snapshot()
            .await
            .map_err(super::claude_pty::map_herdr)?;
        Ok((facet_ok, snapshot))
    }

    /// Ownership of this resource's workspace id against a fresh audit.
    fn workspace_ownership(&self, facet_ok: bool, snapshot: &SessionSnapshot) -> Ownership {
        match snapshot
            .workspaces
            .iter()
            .find(|w| w.workspace_id == self.workspace_id)
        {
            None => Ownership::Absent,
            Some(workspace) if workspace.label == self.workspace_label && facet_ok => {
                Ownership::Ours
            }
            Some(_) => Ownership::Foreign,
        }
    }

    /// Ownership of an ARBITRARY pane id (usually one this resource recorded)
    /// against a fresh audit. The pane is ours only while it lives under our
    /// workspace id, that workspace still carries our creation label, and the
    /// server boot is unchanged.
    fn pane_ownership(
        &self,
        pane_id: &str,
        facet_ok: bool,
        snapshot: &SessionSnapshot,
    ) -> Ownership {
        let Some(pane) = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return Ownership::Absent;
        };
        if facet_ok
            && pane.workspace_id == self.workspace_id
            && snapshot.workspaces.iter().any(|workspace| {
                workspace.workspace_id == self.workspace_id
                    && workspace.label == self.workspace_label
            })
        {
            Ownership::Ours
        } else {
            Ownership::Foreign
        }
    }

    /// Ownership of a tab id against a fresh audit.
    fn tab_ownership(&self, tab_id: &str, facet_ok: bool, snapshot: &SessionSnapshot) -> Ownership {
        let Some(tab) = snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id) else {
            return Ownership::Absent;
        };
        if facet_ok
            && tab.workspace_id == self.workspace_id
            && snapshot.workspaces.iter().any(|workspace| {
                workspace.workspace_id == self.workspace_id
                    && workspace.label == self.workspace_label
            })
        {
            Ownership::Ours
        } else {
            Ownership::Foreign
        }
    }

    /// Refuse outright when a pane id we are about to close is held by a
    /// foreign workspace or a restarted server. An absent pane is fine —
    /// closing it is a no-op.
    async fn refuse_foreign_pane(&self, client: &Client, pane_id: &str) -> DriverResult<()> {
        let (facet_ok, snapshot) = self.audit(client).await?;
        if self.pane_ownership(pane_id, facet_ok, &snapshot) == Ownership::Foreign {
            return Err(self.foreign_refusal(pane_id));
        }
        Ok(())
    }

    /// Same guard before closing the recorded tab.
    async fn refuse_foreign_tab(&self, client: &Client, tab_id: &str) -> DriverResult<()> {
        let (facet_ok, snapshot) = self.audit(client).await?;
        if self.tab_ownership(tab_id, facet_ok, &snapshot) == Ownership::Foreign {
            return Err(self.foreign_refusal(tab_id));
        }
        Ok(())
    }

    #[cold]
    fn foreign_refusal(&self, id: &str) -> DriverError {
        DriverError::ForeignHerdrResource(format!(
            "refusing to close {id}: workspace {} (label {}) is now held by a different \
             Herdr boot; a pane with that reused id belongs to an unrelated workspace",
            self.workspace_id, self.workspace_label
        ))
    }

    /// Panes inside the owned workspace, agent pane first so the native
    /// process is reclaimed before the shell that would outlive it.
    ///
    /// Callers MUST re-validate each pane immediately before closing it:
    /// this snapshot goes stale the moment it returns.
    async fn owned_panes(&self, client: &Client) -> DriverResult<Vec<String>> {
        let (facet_ok, snapshot) = self.audit(client).await?;
        let mut panes: Vec<String> = snapshot
            .panes
            .iter()
            .filter(|pane| {
                self.pane_ownership(&pane.pane_id, facet_ok, &snapshot) == Ownership::Ours
            })
            .map(|pane| pane.pane_id.clone())
            .collect();
        panes.sort_by_key(|pane| *pane != self.pane_id);
        Ok(panes)
    }

    /// Ask the agent to exit, and wait the bounded grace for it to do so.
    /// Returns true only when the agent was OBSERVED leaving from a
    /// still-owned workspace; a timeout, a Herdr RPC failure, or a workspace
    /// that changed ownership underneath us (a restart with id reuse) reports
    /// false, so the caller runs the ownership-revalidating forced termination
    /// instead of draining against a writer that may still be alive
    /// (c-ctxusage r13 item 1, r14 item 1).
    async fn await_polite_exit(&self, client: &Client, grace: Duration) -> bool {
        /// Result of the grace wait.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Polite {
            Gone,
            Foreign,
        }
        let grace_run = async {
            let map = super::claude_pty::map_herdr;
            let _ = client
                .pane_send_keys(&self.pane_id, vec!["ctrl+c".into()])
                .await;
            tokio::time::sleep(Duration::from_millis(150)).await;
            // Re-validate ownership BEFORE the second tap: the server may have
            // restarted on the first Ctrl-C and reused our pane id for an
            // unrelated workspace (c-ctxusage r14 item 1). A second tap, the
            // shell `exit` below and every later keystroke would then land in a
            // foreign user's pane.
            let (facet_ok, snapshot) = self.audit(client).await?;
            match self.workspace_ownership(facet_ok, &snapshot) {
                Ownership::Ours => {
                    client
                        .pane_send_keys(&self.pane_id, vec!["ctrl+c".into()])
                        .await
                        .map_err(map)?;
                }
                Ownership::Absent => return Ok(Polite::Gone),
                Ownership::Foreign => return Ok(Polite::Foreign),
            }
            loop {
                let (facet_ok, snapshot) = self.audit(client).await?;
                match self.workspace_ownership(facet_ok, &snapshot) {
                    Ownership::Absent => return Ok(Polite::Gone),
                    Ownership::Foreign => return Ok(Polite::Foreign),
                    Ownership::Ours => {}
                }
                if !snapshot
                    .agents
                    .iter()
                    .any(|agent| agent.pane_id == self.pane_id)
                {
                    // Only send the shell builtin after the native agent is
                    // gone AND the pane is still ours; never accidentally turn
                    // the word exit into a model prompt, and never into a
                    // foreign shell.
                    if let Some(process) = client
                        .pane_process_info(Some(self.pane_id.clone()))
                        .await
                        .map_err(map)?
                        .process_info
                        && process.shell_pid.is_some()
                        && process
                            .foreground_processes
                            .iter()
                            .all(|p| Some(p.pid) == process.shell_pid)
                    {
                        client
                            .pane_send_text(&self.pane_id, "exit\r")
                            .await
                            .map_err(map)?;
                    }
                    return Ok::<_, DriverError>(Polite::Gone);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        matches!(
            tokio::time::timeout(grace, grace_run).await,
            Ok(Ok(Polite::Gone))
        )
    }

    /// Close the AGENT pane — reaping its foreground process group — and
    /// VERIFY, bounded, that Herdr no longer reports either the agent or the
    /// pane. Every other pane, the tab and the workspace stay alive, as do the
    /// caller's collector and observation sender: the final transcript drain
    /// runs only after this returns (c-ctxusage r13 item 1). The subsequent
    /// [`Self::reap`] tolerates the pane already being gone.
    ///
    /// Ownership is re-proven IMMEDIATELY before the destructive `pane.close`
    /// and again after it (workspace creation label + server boot facet): if
    /// Herdr restarted during the grace and handed `w1:p1` to an unrelated
    /// workspace, the close is refused and the foreign pane survives
    /// (c-ctxusage r14 item 1). There is no verification sleep: a
    /// pre-close `events.subscribe` captures the pane's exit/close event,
    /// awaited under the bounded grace, and one post-event snapshot verifies.
    async fn force_terminate(&self, client: &Client) -> DriverResult<()> {
        // Subscribe FIRST: subscriptions are frozen at request time and the
        // pane still exists here, so the pane.exited/pane.closed event emitted
        // by the close RPC below cannot be missed.
        let mut events = client
            .subscribe(vec![
                Subscription::pane_exited(),
                Subscription::global("pane.closed"),
            ])
            .await
            .map_err(super::claude_pty::map_herdr)?;

        // Ownership proof immediately before the destructive call.
        let (facet_ok, snapshot) = self.audit(client).await?;
        match self.pane_ownership(&self.pane_id, facet_ok, &snapshot) {
            Ownership::Foreign => return Err(self.foreign_refusal(&self.pane_id)),
            Ownership::Absent => return Ok(()),
            Ownership::Ours => {}
        }
        let pane_present = snapshot
            .panes
            .iter()
            .any(|pane| pane.pane_id == self.pane_id);
        let agent_present = snapshot
            .agents
            .iter()
            .any(|agent| agent.pane_id == self.pane_id);
        if !pane_present && !agent_present {
            // The polite exit landed between the grace expiring and here.
            return Ok(());
        }

        // Best effort: the verification snapshot is authoritative even if this
        // RPC was refused, and closing an already-gone pane stays harmless —
        // but only because ownership was proven above.
        let _ = client.pane_close(&self.pane_id).await;

        // Event-driven, bounded: wake on THIS pane's lifecycle event instead
        // of sleeping between verification polls.
        let event_wait = async {
            while let Some(item) = events.next_event().await {
                let Ok(event) = item else { break };
                if matches!(event.kind, EventKind::PaneExited | EventKind::PaneClosed)
                    && event.pane_id().is_some_and(|id| id == self.pane_id)
                {
                    return;
                }
            }
        };
        let _ = tokio::time::timeout(FORCE_EXIT_GRACE, event_wait).await;

        // One authoritative verification snapshot — ownership re-proven, then
        // both the agent and the pane gone.
        let (facet_ok, snapshot) = self.audit(client).await?;
        if self.pane_ownership(&self.pane_id, facet_ok, &snapshot) == Ownership::Foreign {
            return Err(self.foreign_refusal(&self.pane_id));
        }
        let agent_gone = !snapshot
            .agents
            .iter()
            .any(|agent| agent.pane_id == self.pane_id);
        let pane_gone = !snapshot
            .panes
            .iter()
            .any(|pane| pane.pane_id == self.pane_id);
        if agent_gone && pane_gone {
            Ok(())
        } else {
            Err(DriverError::InvalidLaunchSpec(
                "native writer survived forced termination".into(),
            ))
        }
    }
}

/// Node store callback invoked before agent.start and after verified removal.
pub trait PtyResourceStore: Send + Sync {
    /// Persist ownership before dispatching native work.
    fn save(&self, resource: &PtyResource) -> DriverResult<()>;
    /// Forget ownership only after successful cleanup.
    fn remove(&self, resource: &PtyResource) -> DriverResult<()>;
}

type ResourceOwner = (InstanceId, Arc<dyn PtyResourceStore>);

#[derive(Default, Clone)]
pub(crate) struct PtyResources {
    owner: Arc<Mutex<Option<ResourceOwner>>>,
    resources: Arc<tokio::sync::Mutex<Vec<PtyResource>>>,
}

impl PtyResources {
    pub fn configure(&self, id: InstanceId, store: Arc<dyn PtyResourceStore>) {
        if let Ok(mut owner) = self.owner.lock() {
            *owner = Some((id, store));
        }
    }

    // Finish registration even if the instance worker is cancelled while waiting
    // for workspace.create. close() waits on the same resource lock.
    pub async fn create_workspace(
        &self,
        client: &Client,
        session: &str,
        params: WorkspaceCreateParams,
    ) -> DriverResult<WorkspaceCreated> {
        let this = self.clone();
        let client = client.clone();
        let session = session.to_owned();
        let resources = self.resources.clone().lock_owned().await;
        tokio::spawn(async move {
            let mut resources = resources;
            let created = client
                .workspace_create(params)
                .await
                .map_err(super::claude_pty::map_herdr)?;
            // Captured against the SAME server that answered create: the
            // ownership guard before later destructive closes re-proves it
            // (c-ctxusage r14 item 1).
            let facet = client.ping().await.ok().map(|pong| ServerFacet::of(&pong));
            this.record_locked(
                &client,
                &session,
                &created,
                &created.root_pane.pane_id,
                facet,
                &mut resources,
            )?;
            Ok(created)
        })
        .await
        .map_err(|e| DriverError::InvalidLaunchSpec(format!("Herdr allocation task: {e}")))?
    }

    pub async fn record(
        &self,
        client: &Client,
        session: &str,
        created: &WorkspaceCreated,
        pane_id: &str,
    ) -> DriverResult<()> {
        // See `create_workspace`: capture the boot identity while we know the
        // server answering is the one that owns the new workspace.
        let facet = client.ping().await.ok().map(|pong| ServerFacet::of(&pong));
        let mut resources = self.resources.lock().await;
        self.record_locked(client, session, created, pane_id, facet, &mut resources)
    }

    fn record_locked(
        &self,
        client: &Client,
        session: &str,
        created: &WorkspaceCreated,
        pane_id: &str,
        server_facet: Option<ServerFacet>,
        resources: &mut Vec<PtyResource>,
    ) -> DriverResult<()> {
        let owner = self
            .owner
            .lock()
            .map_err(|_| DriverError::ControlUnavailable)?
            .clone();
        let resource = PtyResource {
            instance_id: owner.as_ref().map(|(id, _)| id.clone()),
            socket_path: client.socket_path().to_path_buf(),
            session: session.into(),
            workspace_id: created.workspace.workspace_id.clone(),
            workspace_label: created.workspace.label.clone(),
            server_facet,
            tab_id: created.tab.tab_id.clone(),
            pane_id: pane_id.into(),
        };
        resources.retain(|r| r.key() != resource.key());
        resources.push(resource.clone());
        if let Some((_, store)) = owner {
            store.save(&resource)?;
        }
        Ok(())
    }

    /// Phase 1 of close: stop every owned native writer with a VERIFIED exit
    /// (force-terminating any that outlive the grace), keeping the
    /// panes/workspaces alive so the caller can drain the interrupted turn's
    /// final transcript append before reaping (c-ctxusage r12 item 1 / r13
    /// item 1). Returns each resource's verified stop outcome, in stop order.
    pub async fn interrupt(&self, grace: Duration) -> DriverResult<Vec<InterruptOutcome>> {
        let resources = self.resources.lock().await;
        let mut outcomes = Vec::with_capacity(resources.len());
        for resource in resources.iter().rev() {
            outcomes.push(resource.interrupt(grace).await?);
        }
        Ok(outcomes)
    }

    /// Phase 2 of close: tear down and verify every pane/tab/workspace and
    /// forget ownership. Call ONLY after the final transcript drain
    /// ([`Self::interrupt`] ran first).
    pub async fn reap(&self) -> DriverResult<()> {
        let mut resources = self.resources.lock().await;
        while let Some(resource) = resources.last() {
            resource.reap().await?;
            if let Some((_, store)) = self
                .owner
                .lock()
                .map_err(|_| DriverError::ControlUnavailable)?
                .as_ref()
            {
                store.remove(resource)?;
            }
            resources.pop();
        }
        Ok(())
    }

    pub async fn close(&self) -> DriverResult<()> {
        self.interrupt(AGENT_EXIT_GRACE).await?;
        self.reap().await
    }
}
