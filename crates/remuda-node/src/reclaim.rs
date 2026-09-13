//! Reclaim carrier resources independently of worker lifetime and adopt without replay.

use crate::{
    DevNode, Driver, DriverError, DriverFuture, DriverRequest, LocalStore, NativeDriverConfig,
    NodeError,
};
use remuda_driver::{PtyResource, PtyResourceStore};
use remuda_protocol::{Activity, DriverKind, InstanceId, InstanceLifecycle, Knowledge};
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

pub(crate) struct ResourceStore(pub Arc<dyn LocalStore>);

impl PtyResourceStore for ResourceStore {
    fn save(&self, resource: &PtyResource) -> remuda_driver::DriverResult<()> {
        self.0
            .put_pty_resource(resource)
            .map_err(|e| remuda_driver::DriverError::InvalidLaunchSpec(e.to_string()))
    }
    fn remove(&self, resource: &PtyResource) -> remuda_driver::DriverResult<()> {
        self.0
            .remove_pty_resource(&resource.key())
            .map_err(|e| remuda_driver::DriverError::InvalidLaunchSpec(e.to_string()))
    }
}

/// Close and forget every Herdr carrier this Instance still owns.
///
/// A driver's own `close()` reclaims the panes it holds in memory, but the
/// durable `pty_resources` row is the only record that survives a rebuilt
/// driver, an adopted pane, or a close that reported an error — and until
/// this runs, that row's pane stays in the operator's Herdr session as an
/// idle orphan (DEFECT B). Idempotent: a row whose workspace is already gone
/// closes trivially and is dropped.
///
/// Best effort: a carrier that will not close keeps its row, so the next
/// startup sweep tries again rather than losing track of it.
pub(crate) async fn reclaim_instance_carriers(store: &dyn LocalStore, instance_id: &InstanceId) {
    let resources = match store.pty_resources() {
        Ok(resources) => resources,
        Err(error) => {
            tracing::warn!(%error, "could not list carrier ownership after close");
            return;
        }
    };
    for resource in resources
        .iter()
        .filter(|r| r.instance_id.as_ref() == Some(instance_id))
    {
        match resource.close().await {
            Ok(()) => {
                if let Err(error) = store.remove_pty_resource(&resource.key()) {
                    tracing::warn!(%error, "carrier closed but ownership was not dropped");
                }
            }
            Err(error) => tracing::warn!(
                %error,
                instance_id = %instance_id.as_id(),
                workspace_id = %resource.workspace_id,
                "carrier did not close; ownership kept for the next sweep"
            ),
        }
    }
}

impl DevNode {
    /// Configure the isolated Herdr session to reconcile at startup.
    pub fn with_herdr_config(mut self, config: NativeDriverConfig) -> Result<Self, NodeError> {
        let inner = Arc::get_mut(&mut self.inner).ok_or_else(|| {
            NodeError::InvalidConfig("configure Herdr before sharing the Node runtime".into())
        })?;
        inner.herdr_config = Some(config);
        Ok(self)
    }

    /// Adopt known live panes and sweep orphans before accepting new commands.
    /// No prompt, approval, start, or semantic resume is replayed here.
    pub async fn reconcile_herdr(&self) -> Result<(), NodeError> {
        let Some(config) = &self.inner.herdr_config else {
            return Ok(());
        };
        if config.herdr_session.is_empty() || config.herdr_session == "default" {
            return Err(NodeError::InvalidConfig(
                "Node reclamation requires an isolated Herdr session".into(),
            ));
        }
        let socket = config
            .herdr_socket_dir
            .as_ref()
            .map(|dir| dir.join("herdr.sock"))
            .unwrap_or_else(|| remuda_herdr::session_sockets(&config.herdr_session).api);
        let resources = self.inner.store.pty_resources()?;
        if !socket.exists() {
            for resource in resources.iter().filter(|r| r.socket_path == socket) {
                self.inner.store.remove_pty_resource(&resource.key())?;
                if let Some(id) = &resource.instance_id {
                    self.mark_exited(id, "carrier-missing")?;
                }
            }
            return Ok(());
        }
        let client = remuda_herdr::Client::connect(&socket).with_timeout(Duration::from_secs(2));
        let snapshot = client.session_snapshot().await.map_err(node_error)?;
        let agents = client.agent_list().await.map_err(node_error)?.agents;
        let mut adopted = BTreeSet::new();
        let mut adopted_panes = BTreeSet::new();
        for resource in resources.iter().filter(|r| r.socket_path == socket) {
            let instance = resource
                .instance_id
                .as_ref()
                .and_then(|id| self.inner.store.get_instance(id).ok());
            let present = snapshot.workspaces.iter().any(|w| {
                w.workspace_id == resource.workspace_id && w.label == resource.workspace_label
            });
            let live = present
                && agents
                    .iter()
                    .any(|a| a.pane_id == resource.pane_id && a.agent.is_some());
            if let Some(instance) =
                instance.filter(|i| i.lifecycle == InstanceLifecycle::Ready && live)
            {
                adopted.insert(resource.workspace_id.clone());
                adopted_panes.insert(resource.pane_id.clone());
                self.inner.store.mark_unsettled_unknown(&instance.meta.id)?;
                self.adopt_worker(
                    instance.meta.id.clone(),
                    Arc::new(AdoptedPty {
                        resource: resource.clone(),
                        kind: instance.driver,
                        store: self.inner.store.clone(),
                    }),
                )
                .await;
                tracing::info!(instance_id = %instance.meta.id.as_id(), "adopted live Herdr pane without replay");
            } else if config.herdr_orphan_sweep {
                // Reclaim everything else: an Instance that is exited, failed,
                // or whose agent is gone owns no pane. One stubborn carrier
                // must not abort the sweep and leave the other orphans behind
                // — the workspace pass below is the backstop for this one.
                match resource.close().await {
                    Ok(()) => {
                        self.inner.store.remove_pty_resource(&resource.key())?;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            workspace_id = %resource.workspace_id,
                            "orphan carrier did not close; ownership kept for the next sweep"
                        );
                    }
                }
                if let Some(id) = &resource.instance_id {
                    self.mark_exited(id, "startup-orphan")?;
                }
            }
        }
        if config.herdr_orphan_sweep {
            for workspace in snapshot.workspaces {
                if adopted.contains(&workspace.workspace_id) {
                    // Only the adopted agent survives. Its old root shell and any
                    // unrelated panes in the workspace are no longer needed.
                    for pane in snapshot.panes.iter().filter(|p| {
                        p.workspace_id == workspace.workspace_id
                            && !adopted_panes.contains(&p.pane_id)
                    }) {
                        client.pane_close(&pane.pane_id).await.map_err(node_error)?;
                    }
                    continue;
                }
                // A resource close above may already have removed this workspace.
                if client
                    .workspace_list()
                    .await
                    .map_err(node_error)?
                    .workspaces
                    .iter()
                    .any(|w| w.workspace_id == workspace.workspace_id && w.label == workspace.label)
                {
                    // Close the panes first: a workspace whose agent ignores
                    // the request would otherwise survive and leave an idle
                    // pane in the operator's session (DEFECT B).
                    for pane in snapshot
                        .panes
                        .iter()
                        .filter(|pane| pane.workspace_id == workspace.workspace_id)
                    {
                        let _ = client.pane_close(&pane.pane_id).await;
                    }
                    client
                        .workspace_close(&workspace.workspace_id)
                        .await
                        .map_err(node_error)?;
                }
            }
        }
        Ok(())
    }

    /// Stop workers, close every retained driver and sweep durable ownership.
    /// Finished workers count as settled even when their command queue is gone.
    pub async fn shutdown(&self) -> Result<(), NodeError> {
        self.inner.stopping.store(true, Ordering::SeqCst);
        let _mutations = self.inner.mutations.write().await;
        let workers = std::mem::take(&mut *self.inner.workers.lock().await);
        for worker in workers.values() {
            worker.abort();
        }
        for (_, worker) in workers {
            let _ = worker.await;
        }
        let mut tasks = tokio::task::JoinSet::new();
        for instance in self.inner.store.list_instances()? {
            let node = self.clone();
            tasks.spawn(async move {
                node.close_ended_instance(&instance.meta.id, "node-shutdown")
                    .await
            });
        }
        let mut failure = None;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failure = Some(error),
                Err(error) => failure = Some(node_error(error)),
            }
        }
        // Includes partial launches which never yielded a driver handle.
        for resource in self.inner.store.pty_resources()? {
            match resource.close().await {
                Ok(()) => self.inner.store.remove_pty_resource(&resource.key())?,
                Err(error) => failure = Some(node_error(error)),
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn close_ended_instance(
        &self,
        id: &InstanceId,
        reason: &str,
    ) -> Result<(), NodeError> {
        let driver = self.inner.instance_drivers.read().await.get(id).cloned();
        if let Some(driver) = driver {
            driver
                .execute(DriverRequest::Close)
                .await
                .map_err(node_error)?;
        }
        // Best effort per carrier: one that will not close must not stop the
        // others from being reclaimed, nor leave the Instance looking live.
        reclaim_instance_carriers(self.inner.store.as_ref(), id).await;
        self.mark_exited(id, reason)
    }

    fn mark_exited(&self, id: &InstanceId, reason: &str) -> Result<(), NodeError> {
        let instance = self.inner.store.get_instance(id)?;
        if instance.lifecycle == InstanceLifecycle::Exited {
            return Ok(());
        }
        self.inner.store.mark_unsettled_unknown(id)?;
        self.inner.store.set_instance_state(
            id,
            Some(InstanceLifecycle::Exited),
            Some(Knowledge::Known {
                value: Activity::Idle,
            }),
        )?;
        crate::runtime::append_instance_lifecycle(
            self.inner.store.as_ref(),
            id,
            None,
            "exited",
            reason,
        )
    }
}

struct AdoptedPty {
    store: Arc<dyn LocalStore>,
    resource: PtyResource,
    kind: DriverKind,
}

impl Driver for AdoptedPty {
    fn kind(&self) -> DriverKind {
        self.kind
    }
    fn execute(&self, request: DriverRequest) -> DriverFuture<'_> {
        Box::pin(async move {
            let client = self.resource.client();
            let pane = &self.resource.pane_id;
            match request {
                DriverRequest::Close => {
                    self.resource
                        .close()
                        .await
                        .map_err(|e| DriverError::Failed(e.to_string()))?;
                    self.store
                        .remove_pty_resource(&self.resource.key())
                        .map_err(|e| DriverError::Failed(e.to_string()))?;
                }
                DriverRequest::Send { prompt, .. } => {
                    client
                        .agent_prompt(remuda_herdr::AgentPromptParams {
                            target: pane.clone(),
                            text: prompt,
                            wait: None,
                        })
                        .await
                        .map_err(|e| DriverError::Failed(e.to_string()))?;
                }
                DriverRequest::SendKeys { keys } => {
                    client
                        .agent_send_keys(pane, keys)
                        .await
                        .map_err(|e| DriverError::Failed(e.to_string()))?;
                }
                DriverRequest::Cancel => {
                    client
                        .agent_send_keys(pane, vec!["esc".into()])
                        .await
                        .map_err(|e| DriverError::Failed(e.to_string()))?;
                }
                DriverRequest::RespondInteraction { .. } => {
                    return Err(DriverError::Unsupported(
                        "adopted PTY requires native TTY interaction".into(),
                    ));
                }
                DriverRequest::Configure { .. } => {
                    return Ok(vec![crate::driver::DriverEmission::NativeLifecycle {
                        name: "instance.configure".into(),
                        status: "accepted-noop: adopted PTY has no runtime effort command".into(),
                        severity: remuda_protocol::Severity::Info,
                    }]);
                }
            }
            Ok(Vec::new())
        })
    }
}

fn node_error(error: impl std::fmt::Display) -> NodeError {
    NodeError::Driver(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DevServerConfig, DriverRegistry, MemoryStore};
    use remuda_herdr::{AgentStartParams, Client, WorkspaceCreateParams};
    use remuda_protocol::{CommandState, HostId, WorkspaceId};
    use remuda_testing::{FakeHerdrOptions, FakeHerdrServer};

    async fn resource(
        client: &Client,
        instance_id: Option<InstanceId>,
        label: &str,
    ) -> PtyResource {
        let created = client
            .workspace_create(WorkspaceCreateParams {
                label: Some(label.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let pane_id = created.root_pane.pane_id;
        client
            .agent_start(AgentStartParams {
                name: format!("agent-{}", created.workspace.workspace_id),
                kind: "claude".into(),
                pane_id: pane_id.clone(),
                args: vec![],
                timeout_ms: Some(500),
            })
            .await
            .unwrap();
        PtyResource {
            instance_id,
            socket_path: client.socket_path().into(),
            session: "remuda-reclaim-test".into(),
            workspace_id: created.workspace.workspace_id,
            workspace_label: created.workspace.label,
            tab_id: created.tab.tab_id,
            pane_id,
        }
    }

    fn node(store: Arc<MemoryStore>, config: NativeDriverConfig) -> DevNode {
        DevNode::with_parts(
            &DevServerConfig::loopback(0),
            store,
            DriverRegistry::with_fake().unwrap(),
        )
        .unwrap()
        .with_herdr_config(config)
        .unwrap()
    }

    #[tokio::test]
    async fn restart_adopts_live_pane_sweeps_orphan_and_shutdown_cleans_durable_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("herdr");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let socket = socket_dir.join("herdr.sock");
        let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();
        let client = Client::connect(socket);
        let store = Arc::new(MemoryStore::open_journaled(dir.path().join("node"), 64).unwrap());
        let id = InstanceId::new();
        store
            .insert_instance(
                crate::runtime::fixture_instance(
                    id.clone(),
                    HostId::new(),
                    WorkspaceId::new(),
                    DriverKind::GenericPty,
                )
                .unwrap(),
            )
            .unwrap();
        let live = resource(&client, Some(id.clone()), "remuda-known").await;
        store.put_pty_resource(&live).unwrap();
        resource(&client, None, "remuda-orphan").await;
        drop(store);
        let store = Arc::new(MemoryStore::open_journaled(dir.path().join("node"), 64).unwrap());
        assert_eq!(
            store.pty_resources().unwrap().len(),
            1,
            "ownership survives restart"
        );
        let mut config = NativeDriverConfig::new(dir.path().join("node"));
        config.herdr_socket_dir = Some(socket_dir);
        config.herdr_session = "remuda-reclaim-test".into();
        let node = node(store.clone(), config);
        node.reconcile_herdr().await.unwrap();
        let snapshot = client.session_snapshot().await.unwrap();
        assert_eq!(snapshot.panes.len(), 1);
        assert_eq!(snapshot.panes[0].pane_id, live.pane_id);
        // Adoption gives the known instance a command worker, without starting or replaying.
        let command = node
            .submit_command(
                &id,
                serde_json::from_value(
                    serde_json::json!({"operation":"send", "prompt":"explicit followup"}),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .command;
        tokio::time::timeout(Duration::from_secs(2), async {
            while node.get_command(&command.command_id).unwrap().state != CommandState::Settled {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        node.shutdown().await.unwrap();
        node.shutdown().await.unwrap();
        assert!(
            client
                .session_snapshot()
                .await
                .unwrap()
                .workspaces
                .is_empty()
        );
        assert!(store.pty_resources().unwrap().is_empty());
        assert_eq!(
            store.get_instance(&id).unwrap().lifecycle,
            InstanceLifecycle::Exited
        );
    }

    #[tokio::test]
    async fn opt_out_preserves_unknown_panes_but_shutdown_reclaims_owned_partial_launch() {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("herdr");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let socket = socket_dir.join("herdr.sock");
        let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();
        let client = Client::connect(socket);
        let store = Arc::new(MemoryStore::new(64));
        let id = InstanceId::new();
        let mut instance = crate::runtime::fixture_instance(
            id.clone(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::GenericPty,
        )
        .unwrap();
        instance.lifecycle = InstanceLifecycle::Failed;
        store.insert_instance(instance).unwrap();
        let owned = resource(&client, Some(id.clone()), "remuda-partial").await;
        store.put_pty_resource(&owned).unwrap();
        let orphan = resource(&client, None, "manual-recovery").await;
        let mut config = NativeDriverConfig::new(dir.path().into());
        config.herdr_socket_dir = Some(socket_dir);
        config.herdr_orphan_sweep = false;
        let node = node(store.clone(), config);
        node.reconcile_herdr().await.unwrap();
        assert_eq!(client.session_snapshot().await.unwrap().workspaces.len(), 2);
        // There is no driver task at all, matching the shutdown regression.
        node.shutdown().await.unwrap();
        let snapshot = client.session_snapshot().await.unwrap();
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].workspace_id, orphan.workspace_id);
        assert_eq!(
            store.get_instance(&id).unwrap().lifecycle,
            InstanceLifecycle::Exited
        );
    }

    /// DEFECT B: a stop settled, but the agent's Herdr pane stayed in the
    /// operator's session. `fake-herdr` never drops an agent on `ctrl+c`, so
    /// this is exactly the stubborn-agent case: closing must still reclaim the
    /// pane, the tab and the workspace once the grace expires.
    #[tokio::test]
    async fn stopping_a_pty_instance_closes_its_agent_pane_and_forgets_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();
        let client = Client::connect(&socket);
        let store = Arc::new(MemoryStore::new(64));
        let id = InstanceId::new();
        store
            .insert_instance(
                crate::runtime::fixture_instance(
                    id.clone(),
                    HostId::new(),
                    WorkspaceId::new(),
                    DriverKind::ClaudePty,
                )
                .unwrap(),
            )
            .unwrap();
        let owned = resource(&client, Some(id.clone()), "remuda-stopped").await;
        store.put_pty_resource(&owned).unwrap();
        // A second Instance's pane proves the reclaim is scoped to one owner.
        let other = resource(&client, None, "someone-else").await;
        assert_eq!(client.session_snapshot().await.unwrap().panes.len(), 2);
        assert_eq!(client.agent_list().await.unwrap().agents.len(), 2);

        reclaim_instance_carriers(store.as_ref(), &id).await;

        let snapshot = client.session_snapshot().await.unwrap();
        assert_eq!(
            snapshot.workspaces.len(),
            1,
            "only the stopped Instance's workspace is closed"
        );
        assert_eq!(snapshot.workspaces[0].workspace_id, other.workspace_id);
        assert_eq!(snapshot.panes.len(), 1, "the stopped agent's pane is gone");
        assert_eq!(snapshot.panes[0].pane_id, other.pane_id);
        let agents = client.agent_list().await.unwrap().agents;
        assert_eq!(agents.len(), 1, "the stopped agent is no longer listed");
        assert!(
            store
                .pty_resources()
                .unwrap()
                .iter()
                .all(|r| r.instance_id.as_ref() != Some(&id))
        );

        // Idempotent: a second stop of the same Instance is a no-op.
        reclaim_instance_carriers(store.as_ref(), &id).await;
        assert_eq!(client.session_snapshot().await.unwrap().panes.len(), 1);
    }

    #[tokio::test]
    async fn stale_workspace_id_never_closes_a_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap();
        let client = Client::connect(socket);
        let mut stale = resource(&client, None, "replacement").await;
        stale.workspace_label = "previous-server-resource".into();
        stale.close().await.unwrap();
        assert_eq!(client.session_snapshot().await.unwrap().workspaces.len(), 1);
    }
}
