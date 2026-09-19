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

/// Reason and diagnostic name for a session lost to a Node restart (§8 plan A).
///
/// Shared with the Hub, which applies the same string when its own reconcile
/// notices an epoch change: the web should not have to know whether the Node
/// confessed first or the Hub worked it out.
pub const NODE_EPOCH_CHANGED: &str = "node-epoch-changed";

/// The journal entry the web turns into 「Node 重启，会话已结束」 plus Resume.
fn node_epoch_diagnostic() -> remuda_protocol::ObservationPayload {
    remuda_protocol::ObservationPayload::Lifecycle(Box::new(
        remuda_protocol::LifecyclePayload::Native(Box::new(remuda_protocol::NativeLifecycle {
            topic: remuda_protocol::LifecycleTopic::Diagnostic,
            native_name: "node_epoch_changed".to_owned(),
            native_id: remuda_protocol::Knowledge::NotApplicable,
            status: remuda_protocol::Knowledge::Known {
                value: "exited".to_owned(),
            },
            related_ids: [
                ("reason".to_owned(), NODE_EPOCH_CHANGED.to_owned()),
                // D-026 can continue this conversation in a fresh PTY, so the
                // UI should offer that rather than only reporting the loss.
                ("resumable".to_owned(), "true".to_owned()),
            ]
            .into_iter()
            .collect(),
            data_ref: None,
            // Not an error: the Node restarted, which is a normal thing to do.
            // §8 asks for an honest explanation, not an incident.
            severity: remuda_protocol::Severity::Warning,
            affects_completion: true,
        })),
    ))
}

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

    /// Close one instance's herdr carrier (tab/panes/workspace) at worker
    /// retire (M1 batch 5a). Best-effort and idempotent: print-driver workers
    /// and already-gone carriers report success. Only touches the resource the
    /// Node itself recorded for `instance_id`.
    pub(crate) async fn close_instance_carrier(&self, instance_id: &str) -> Result<(), NodeError> {
        let Some(config) = &self.inner.herdr_config else {
            return Ok(());
        };
        let socket = crate::carrier_recovery::herdr_socket(config);
        if !socket.exists() {
            return Ok(());
        }
        let parsed: InstanceId = instance_id.parse()?;
        let resources = self.inner.store.pty_resources()?;
        let Some(resource) = resources
            .into_iter()
            .find(|resource| resource.instance_id.as_ref() == Some(&parsed))
        else {
            return Ok(());
        };
        resource
            .close()
            .await
            .map_err(|error| NodeError::InvalidRequest(format!("close worker carrier: {error}")))?;
        self.inner.store.remove_pty_resource(&resource.key())?;
        Ok(())
    }

    /// Adopt known live panes and sweep orphans before accepting new commands.
    /// No prompt, approval, start, or semantic resume is replayed here.
    ///
    /// Settles native-PTY sessions **first** (§8 plan A,
    /// [`Self::reconcile_native_pty`]), then waits on herdr. The order is not
    /// cosmetic: waiting out a predecessor's session server can take the whole
    /// of [`remuda_herdr::RetryPolicy`]'s budget, and until it returns every
    /// native-PTY row still reads `ready`. Those sessions are already gone and
    /// the user is already looking at them, so their loss is reported before
    /// anything that can block — a herdr socket that will not answer must not
    /// also hide an unrelated carrier's casualties.
    pub async fn reconcile_herdr(&self) -> Result<(), NodeError> {
        self.reconcile_native_pty().await?;
        // Pin the hook relay now, so the copy captures the build the Node is
        // running rather than whatever lands before the first hooked launch,
        // then sweep pinned copies no surviving instance references.
        self.pin_hook_relay();
        self.collect_hook_relays();
        self.reconcile_herdr_with(remuda_herdr::RetryPolicy::default())
            .await
    }

    /// [`Self::reconcile_herdr`] with an explicit budget for waiting out a
    /// previous Node's session server.
    pub(crate) async fn reconcile_herdr_with(
        &self,
        policy: remuda_herdr::RetryPolicy,
    ) -> Result<(), NodeError> {
        let Some(config) = &self.inner.herdr_config else {
            return Ok(());
        };
        if config.herdr_session.is_empty() || config.herdr_session == "default" {
            return Err(NodeError::InvalidConfig(
                "Node reclamation requires an isolated Herdr session".into(),
            ));
        }
        let socket = crate::carrier_recovery::herdr_socket(config);
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
        // A socket left by the *previous* Node may still be attached to a
        // server that is shutting down: it answers `ping` but refuses real
        // work with `server_unavailable`. Reconciling against it used to abort
        // Node startup entirely, so wait it out and treat the panes as gone.
        let snapshot = match wait_for_snapshot(&client, policy).await {
            Ok(snapshot) => snapshot,
            Err(CarrierWait::Gone) => {
                tracing::warn!(
                    socket = %socket.display(),
                    "previous herdr server shut down during reconciliation; its panes are gone"
                );
                for resource in resources.iter().filter(|r| r.socket_path == socket) {
                    self.inner.store.remove_pty_resource(&resource.key())?;
                    if let Some(id) = &resource.instance_id {
                        self.mark_exited(id, "carrier-shutdown")?;
                    }
                }
                return Ok(());
            }
            Err(CarrierWait::Failed(error)) => return Err(error),
        };
        // Same race one call later: the server can finish exiting between the
        // snapshot and this read.
        let agents = match client.agent_list().await {
            Ok(list) => list.agents,
            Err(error) if error.is_transient_carrier() || error.is_disconnect() => {
                tracing::warn!(
                    socket = %socket.display(),
                    "herdr server exited between snapshot and agent list; treating its panes as gone"
                );
                for resource in resources.iter().filter(|r| r.socket_path == socket) {
                    self.inner.store.remove_pty_resource(&resource.key())?;
                    if let Some(id) = &resource.instance_id {
                        self.mark_exited(id, "carrier-shutdown")?;
                    }
                }
                return Ok(());
            }
            Err(error) => return Err(node_error(error)),
        };
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

    /// Settle every session that did not survive this Node's restart
    /// (D-028 §8, plan A).
    ///
    /// An in-process driver dies with the Node: an in-process `portable-pty`
    /// loses its child to `SIGHUP` when the master fd closes, and a stdio
    /// driver's process is a direct child. There is no adopting either back.
    /// Plan A accepts that and requires the loss be *said out loud* rather
    /// than left as a row that claims to be ready — so every such row from the
    /// previous process is marked exited with `node-epoch-changed`, and a
    /// diagnostic carries the same reason into the journal for the web to
    /// render as 「Node 重启，会话已结束」 beside a Resume affordance.
    ///
    /// This sweeps **every** driver, not `shell-pty` alone. Restricting it to
    /// in-process PTYs left `claude-pty`, `claude-bg`, `codex-appserver` and
    /// `grok-acp` rows reading `running` forever after a restart: they hold
    /// placement slots nothing can release, `remuda watch` keeps calling them
    /// working, and the web terminal keeps showing a frozen screen.
    ///
    /// Two things are exempt, and each is a carrier that legitimately outlives
    /// this process:
    /// * a driver in [`Self::instance_drivers`] was built by *this* process,
    ///   so it is alive and not a restart casualty;
    /// * a row with a durable `pty_resources` entry is Herdr-carried. Those
    ///   panes really do survive the Node and are [`Self::reconcile_herdr`]'s
    ///   business — it adopts the live ones and settles the rest, and it runs
    ///   *after* this so the resource rows are still here to tell the two
    ///   apart.
    ///
    /// Resume is what makes this honest rather than merely blunt: D-026 can
    /// continue the same conversation, and §5.6 makes that a new PTY with
    /// `--resume` prefilled. The session is over; the conversation is not.
    ///
    /// Runs at startup, before any instance is served. Idempotent: a row that
    /// is already exited is left alone, so a Node that restarts twice does not
    /// journal the loss twice.
    pub async fn reconcile_native_pty(&self) -> Result<(), NodeError> {
        let herdr_carried: BTreeSet<String> = self
            .inner
            .store
            .pty_resources()?
            .into_iter()
            .filter_map(|resource| resource.instance_id.map(|id| id.as_id().to_string()))
            .collect();
        let mut lost = Vec::new();
        for instance in self.inner.store.list_instances()? {
            if matches!(
                instance.lifecycle,
                InstanceLifecycle::Exited | InstanceLifecycle::Failed
            ) {
                continue;
            }
            // A driver in this map was built by *this* process, so its PTY is
            // alive and this is not a restart casualty. At startup the map is
            // empty; the check matters if this is ever called again later.
            if self
                .inner
                .instance_drivers
                .read()
                .await
                .contains_key(&instance.meta.id)
            {
                continue;
            }
            // Herdr-carried: the pane may still exist and be adoptable, so its
            // row is not this sweep's to settle. Doing it here would journal a
            // 「会话已结束」 for a session the operator is still looking at.
            if herdr_carried.contains(instance.meta.id.as_id().as_str()) {
                continue;
            }
            lost.push(instance.meta.id.clone());
        }
        for id in lost {
            tracing::warn!(
                instance = %id.as_id(),
                "session did not survive the node restart; marking exited ({NODE_EPOCH_CHANGED})"
            );
            self.inner
                .store
                .set_instance_failure(&id, NODE_EPOCH_CHANGED)?;
            // The diagnostic is what the web reads: the lifecycle event says
            // the session ended, and this says *why* in a form that can be
            // turned into a sentence and a Resume button. Journaled *before*
            // the row is settled, so a reader that sees `exited` always finds
            // the explanation already there — settling first leaves a window
            // where a session has ended and nothing can say why.
            if let Err(error) = self.inner.store.append_observation(
                &id,
                None,
                remuda_protocol::Completeness::Structured,
                node_epoch_diagnostic(),
            ) {
                tracing::warn!(%error, "node-epoch-changed diagnostic not journaled");
            }
            self.mark_exited(&id, NODE_EPOCH_CHANGED)?;
        }
        Ok(())
    }

    /// Kill this Node's PTY processes without settling any row.
    ///
    /// What a Node *dying* looks like from the store's point of view: the
    /// processes go, and every row it was serving is left exactly as it was,
    /// still saying `ready`. That gap is the thing
    /// [`Self::reconcile_native_pty`] exists to close, so a test for it has to
    /// be able to produce the gap — going through `instance.close` would settle
    /// the rows and leave reconciliation with nothing to find.
    ///
    /// Not a shutdown: no `stopping` flag, no command settlement, no carrier
    /// sweep. Only the child processes end.
    #[doc(hidden)]
    pub async fn shutdown_processes_only(&self) {
        let workers = std::mem::take(&mut *self.inner.workers.lock().await);
        for worker in workers.values() {
            worker.abort();
        }
        // Awaited, not just aborted. `abort()` schedules cancellation; a worker
        // already inside a close runs to the end of that call and settles the
        // row as `exited`. That is the correct behaviour for a real close and
        // exactly wrong here, where the whole point is to leave the row saying
        // `ready` so reconciliation has the casualty to find.
        for (_, worker) in workers {
            let _ = worker.await;
        }
        // Before the drivers, not after: closing a driver makes it emit its
        // §5.5 exit, and a pump still running would journal that — into a store
        // this Node is about to stop owning.
        self.stop_pumps().await;
        let drivers: Vec<_> = self
            .inner
            .instance_drivers
            .read()
            .await
            .values()
            .cloned()
            .collect();
        for driver in drivers {
            let _ = driver.execute(DriverRequest::Close).await;
        }
        self.inner.instance_drivers.write().await.clear();
        // Same rationale as `shutdown`: the abort skipped worker termination.
        if let Ok(instances) = self.inner.store.list_instances() {
            for instance in instances {
                self.inner
                    .api_relay
                    .revoke_instance(instance.meta.id.as_id().as_str());
            }
        }
    }

    /// Abort every observation pump and wait for it to actually stop.
    ///
    /// A pump owns an `Arc<dyn LocalStore>` and writes through it. Left running
    /// past its Node it keeps that store — and so the journal's SQLite handle
    /// and its in-memory `durable_seq` mirror — alive while a *second* Node has
    /// reopened the same data dir. Both then read the same watermark and
    /// allocate the same sequence number, which SQLite rejects as
    /// `UNIQUE constraint failed: events.instance_id, events.seq`. That is a
    /// restart-shaped race, so it showed up first in the restart test, but any
    /// two Nodes over one data dir can hit it.
    ///
    /// Awaited rather than fired and forgotten: `abort()` only schedules
    /// cancellation, and a pump already inside `append_observation` runs to the
    /// end of that call. Returning before it does would leave exactly the
    /// overlap this exists to prevent.
    async fn stop_pumps(&self) {
        let pumps = std::mem::take(&mut *self.inner.pumps.lock().await);
        for pump in pumps.values() {
            pump.abort();
        }
        for (_, pump) in pumps {
            let _ = pump.await;
        }
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
        self.stop_pumps().await;
        // Workers were aborted, so their terminal relay revocation never ran:
        // shut every per-instance listener and fail every in-flight stream.
        for instance in self.inner.store.list_instances()? {
            self.inner
                .api_relay
                .revoke_instance(instance.meta.id.as_id().as_str());
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

/// Why a bounded snapshot wait did not produce a snapshot.
enum CarrierWait {
    /// The predecessor finished shutting down (or never came back): there is
    /// nothing to adopt.
    Gone,
    /// A genuine failure the caller should surface.
    Failed(NodeError),
}

/// Read a session snapshot, waiting out a predecessor that is shutting down.
///
/// Returns [`CarrierWait::Gone`] once the endpoint stops answering at all,
/// which is the normal end of a restart race: the old server exited and took
/// its panes with it.
async fn wait_for_snapshot(
    client: &remuda_herdr::Client,
    policy: remuda_herdr::RetryPolicy,
) -> Result<remuda_herdr::SessionSnapshot, CarrierWait> {
    let started = std::time::Instant::now();
    let mut attempt = 0u32;
    loop {
        match client.session_snapshot().await {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) if error.is_server_unavailable() => {
                let Some(backoff) = policy.backoff(attempt, started.elapsed()) else {
                    tracing::warn!(
                        waited_secs = policy.max_wait().as_secs(),
                        "previous herdr server never finished shutting down"
                    );
                    return Err(CarrierWait::Gone);
                };
                tokio::time::sleep(backoff).await;
                attempt = attempt.saturating_add(1);
            }
            // The socket went away mid-wait, or the server closed the
            // connection on its way out. `session.snapshot` is read-only, so
            // unlike a prompt there is nothing to be uncertain about: the
            // predecessor is gone and took its panes with it.
            Err(error) if error.is_transient_carrier() || error.is_disconnect() => {
                return Err(CarrierWait::Gone);
            }
            Err(error) => return Err(CarrierWait::Failed(node_error(error))),
        }
    }
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
            &DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
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
