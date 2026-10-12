//! Ownership and bounded reclamation of Herdr runtime resources (never native homes).

use crate::{DriverError, DriverResult};
use remuda_herdr::{Client, WorkspaceCreateParams, WorkspaceCreated};
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
            let _ = client.pane_close(&pane).await;
        }
        // The tab reclaims the root shell left behind by `pane.split`.
        let _ = client.tab_close(&self.tab_id).await;
        if self.owned_workspace_present(&client).await? {
            let _ = client.workspace_close(&self.workspace_id).await;
        }
        if self.owned_workspace_present(&client).await? {
            return Err(DriverError::InvalidLaunchSpec(
                "owned Herdr workspace survived close".into(),
            ));
        }
        let surviving = self.owned_panes(&client).await?;
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

    /// Is this exact workspace — same id *and* creation label — still present?
    ///
    /// Herdr allocates ids deterministically (`w1`, `w1:t1`, `w1:p1`), so a
    /// restarted server hands the same ids to unrelated workspaces. The
    /// `remuda-<uuid>` creation label is the only field that distinguishes
    /// ours, so it stays the ownership test: never close someone else's pane.
    async fn owned_workspace_present(&self, client: &Client) -> DriverResult<bool> {
        let snapshot = client
            .session_snapshot()
            .await
            .map_err(super::claude_pty::map_herdr)?;
        Ok(snapshot
            .workspaces
            .iter()
            .any(|w| w.workspace_id == self.workspace_id && w.label == self.workspace_label))
    }

    /// Panes inside the owned workspace, agent pane first so the native
    /// process is reclaimed before the shell that would outlive it.
    async fn owned_panes(&self, client: &Client) -> DriverResult<Vec<String>> {
        let snapshot = client
            .session_snapshot()
            .await
            .map_err(super::claude_pty::map_herdr)?;
        let mut panes: Vec<String> = snapshot
            .panes
            .iter()
            .filter(|pane| pane.workspace_id == self.workspace_id)
            .map(|pane| pane.pane_id.clone())
            .collect();
        panes.sort_by_key(|pane| *pane != self.pane_id);
        Ok(panes)
    }

    /// Ask the agent to exit, and wait the bounded grace for it to do so.
    /// Returns true only when the agent was OBSERVED leaving: a timeout or a
    /// Herdr RPC failure (the shutdown window refuses every non-ping method)
    /// reports false, so the caller force-terminates and verifies instead of
    /// draining against a writer that may still be alive (c-ctxusage r13
    /// item 1).
    async fn await_polite_exit(&self, client: &Client, grace: Duration) -> bool {
        let grace_run = async {
            let _ = client
                .pane_send_keys(&self.pane_id, vec!["ctrl+c".into()])
                .await;
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = client
                .pane_send_keys(&self.pane_id, vec!["ctrl+c".into()])
                .await;
            loop {
                if !client
                    .agent_list()
                    .await?
                    .agents
                    .iter()
                    .any(|a| a.pane_id == self.pane_id)
                {
                    // Only send the shell builtin after the native agent is gone;
                    // never accidentally turn the word exit into a model prompt.
                    if let Some(process) = client
                        .pane_process_info(Some(self.pane_id.clone()))
                        .await?
                        .process_info
                        && process.shell_pid.is_some()
                        && process
                            .foreground_processes
                            .iter()
                            .all(|p| Some(p.pid) == process.shell_pid)
                    {
                        let _ = client.pane_send_text(&self.pane_id, "exit\r").await;
                    }
                    return Ok::<_, remuda_herdr::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        matches!(tokio::time::timeout(grace, grace_run).await, Ok(Ok(())))
    }

    /// Close the AGENT pane — reaping its foreground process group — and
    /// VERIFY, bounded, that Herdr no longer reports either the agent or the
    /// pane. Every other pane, the tab and the workspace stay alive, as do the
    /// caller's collector and observation sender: the final transcript drain
    /// runs only after this returns (c-ctxusage r13 item 1). The subsequent
    /// [`Self::reap`] tolerates the pane already being gone.
    async fn force_terminate(&self, client: &Client) -> DriverResult<()> {
        // Best effort: the verification snapshot is authoritative even if this
        // RPC was refused, and closing an already-gone pane stays harmless.
        let _ = client.pane_close(&self.pane_id).await;
        let verify = async {
            loop {
                let snapshot = client.session_snapshot().await?;
                let agent_gone = !snapshot
                    .agents
                    .iter()
                    .any(|agent| agent.pane_id == self.pane_id);
                let pane_gone = !snapshot
                    .panes
                    .iter()
                    .any(|pane| pane.pane_id == self.pane_id);
                if agent_gone && pane_gone {
                    return Ok::<_, remuda_herdr::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        match tokio::time::timeout(FORCE_EXIT_GRACE, verify).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(super::claude_pty::map_herdr(error)),
            Err(_) => Err(DriverError::InvalidLaunchSpec(
                "native writer survived forced termination".into(),
            )),
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
            this.record_locked(
                &client,
                &session,
                &created,
                &created.root_pane.pane_id,
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
        let mut resources = self.resources.lock().await;
        self.record_locked(client, session, created, pane_id, &mut resources)
    }

    fn record_locked(
        &self,
        client: &Client,
        session: &str,
        created: &WorkspaceCreated,
        pane_id: &str,
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
