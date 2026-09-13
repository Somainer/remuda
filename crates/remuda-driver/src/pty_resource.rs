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
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(2);

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

    /// Interrupt the agent, then close and verify every pane, tab and the
    /// workspace this Instance owns.
    ///
    /// The interrupt is a courtesy with a bounded grace: an agent that ignores
    /// `ctrl+c` must not keep a pane alive, because the Instance is already
    /// gone as far as the Hub is concerned and the pane would be an orphan in
    /// the operator's Herdr session forever. After the grace the panes are
    /// closed explicitly — closing the tab alone is not enough, since
    /// `pane.split` may have landed the agent outside the recorded tab.
    ///
    /// Success proves carrier removal, never task success.
    pub async fn close(&self) -> DriverResult<()> {
        let client = self.client();
        if !self.owned_workspace_present(&client).await? {
            return Ok(());
        }
        self.interrupt_agent(&client).await;
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

    /// Ask the agent to exit, and wait a bounded time for it to do so.
    ///
    /// Best effort throughout: the caller closes the panes either way, so a
    /// Herdr RPC failure here must not abort reclamation.
    async fn interrupt_agent(&self, client: &Client) {
        let grace = async {
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
        let _ = tokio::time::timeout(AGENT_EXIT_GRACE, grace).await;
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

    pub async fn close(&self) -> DriverResult<()> {
        let mut resources = self.resources.lock().await;
        while let Some(resource) = resources.last() {
            resource.close().await?;
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
}
