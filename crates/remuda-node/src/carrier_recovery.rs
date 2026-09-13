//! Mid-session recovery when the Herdr session server goes away.
//!
//! A Node's Herdr session is named after its data dir, so its server can die
//! (crash, `server.stop`, an operator's `kill`) while the Node keeps running.
//! Before this, the first driver call afterwards surfaced
//! `server_unavailable: server is shutting down` and — via `record_task_exit`
//! on the worker's error path — could take the whole Node process down with it.
//!
//! Recovery here is deliberately narrow. Restarting the session server gives
//! back a *carrier*, not the panes: every pane the old server hosted is gone,
//! along with the native agent processes inside them. So affected Instances are
//! marked failed with a journal diagnostic naming the carrier as the cause,
//! rather than being silently re-launched — the human decides whether to resume.

use crate::{DevNode, LocalStore, NodeError};
use remuda_herdr::RetryPolicy;
use remuda_protocol::{InstanceId, InstanceLifecycle};
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

/// Journal diagnostic name for a carrier that died under a live Instance.
pub(crate) const CARRIER_LOST: &str = "herdr-carrier-lost";

/// Journal diagnostic name for a session server the Node brought back.
pub(crate) const CARRIER_RESTARTED: &str = "herdr-carrier-restarted";

/// What one recovery attempt achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recovery {
    /// Session server is reachable again.
    pub(crate) restarted: bool,
    /// Instances whose panes did not survive.
    pub(crate) affected: Vec<InstanceId>,
}

impl DevNode {
    /// Bring the Herdr session server back after a mid-session loss, and mark
    /// the Instances whose panes went with it.
    ///
    /// Bounded by `policy`: a predecessor still shutting down is waited out,
    /// and a server that never returns leaves the Node running (degraded)
    /// instead of ending the process. Safe to call when nothing is wrong —
    /// a healthy endpoint short-circuits with `restarted: false` and no
    /// journal noise.
    pub async fn recover_herdr_carrier(&self) -> Result<(), NodeError> {
        self.recover_herdr_carrier_with(RetryPolicy::default())
            .await
            .map(|_| ())
    }

    pub(crate) async fn recover_herdr_carrier_with(
        &self,
        policy: RetryPolicy,
    ) -> Result<Recovery, NodeError> {
        let Some(config) = &self.inner.herdr_config else {
            return Ok(Recovery {
                restarted: false,
                affected: Vec::new(),
            });
        };
        let socket = herdr_socket(config);
        // A healthy endpoint means some other error brought us here; do not
        // tear down panes that are still live.
        if carrier_usable(&socket).await {
            return Ok(Recovery {
                restarted: false,
                affected: Vec::new(),
            });
        }

        // Every pane on the lost server is gone, so its ownership rows are
        // stale. Collect them before the restart replaces the socket.
        let affected = self.lost_instances(&socket)?;

        let mut options = remuda_herdr::EnsureOptions::new(
            config.herdr_session.clone(),
            config.herdr_socket_dir.clone(),
        )
        .with_policy(policy);
        // Honour the Node's configured executable. Falling back to `herdr` on
        // PATH here would exec the real binary out from under a test (or an
        // operator) that deliberately pinned a different one.
        if let Some(binary) = &config.herdr_binary {
            options = options.with_binary(binary.clone());
        }
        let restarted = match remuda_herdr::HerdrServer::ensure_with(options).await {
            Ok(server) => {
                if let Some(previous) = server.renamed_from() {
                    tracing::warn!(
                        previous_session = %previous,
                        session = %server.session_name(),
                        "herdr session server restarted under a fresh name"
                    );
                }
                tracing::info!(
                    session = %server.session_name(),
                    socket = %server.socket_path().display(),
                    "herdr session server restarted after mid-session loss"
                );
                true
            }
            Err(error) => {
                // Degraded, not fatal: the Node keeps serving, and the next
                // launch retries the restart.
                tracing::error!(
                    %error,
                    session = %config.herdr_session,
                    "herdr session server could not be restarted; PTY launches stay unavailable"
                );
                false
            }
        };

        for instance_id in &affected {
            self.fail_lost_instance(instance_id, restarted)?;
        }
        Ok(Recovery {
            restarted,
            affected,
        })
    }

    /// Instances that owned a pane on the lost endpoint and had not already
    /// stopped. Their durable ownership rows are dropped along the way: the
    /// panes they name cannot outlive the server that hosted them.
    fn lost_instances(&self, socket: &std::path::Path) -> Result<Vec<InstanceId>, NodeError> {
        let mut affected = BTreeSet::new();
        for resource in self.inner.store.pty_resources()? {
            if resource.socket_path != socket {
                continue;
            }
            if let Some(id) = resource.instance_id.clone()
                && self
                    .inner
                    .store
                    .get_instance(&id)
                    .is_ok_and(|instance| instance.lifecycle != InstanceLifecycle::Exited)
            {
                affected.insert(id);
            }
            self.inner.store.remove_pty_resource(&resource.key())?;
        }
        Ok(affected.into_iter().collect())
    }

    /// Journal why an Instance stopped, then mark it failed.
    ///
    /// The diagnostic is the point: a bare `failed` says nothing about whether
    /// the agent errored or its carrier was pulled out from under it.
    fn fail_lost_instance(
        &self,
        instance_id: &InstanceId,
        restarted: bool,
    ) -> Result<(), NodeError> {
        let store = self.inner.store.as_ref();
        if store
            .get_instance(instance_id)
            .is_ok_and(|instance| instance.lifecycle == InstanceLifecycle::Exited)
        {
            return Ok(());
        }
        append_carrier_diagnostic(
            store,
            instance_id,
            CARRIER_LOST,
            if restarted {
                "herdr session server was lost and restarted; this pane did not survive"
            } else {
                "herdr session server was lost and could not be restarted"
            },
            remuda_protocol::Severity::Error,
        )?;
        if restarted {
            // The restart itself is good news; only the loss is an error.
            append_carrier_diagnostic(
                store,
                instance_id,
                CARRIER_RESTARTED,
                "a fresh herdr session server is available for new instances",
                remuda_protocol::Severity::Info,
            )?;
        }
        store.mark_unsettled_unknown(instance_id)?;
        store.set_instance_failure(instance_id, "herdr-carrier-lost")?;
        crate::runtime::append_instance_lifecycle(
            store,
            instance_id,
            Some("ready"),
            "failed",
            "herdr-carrier-lost",
        )
    }
}

/// Append one error-severity native diagnostic to an Instance journal.
fn append_carrier_diagnostic(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    name: &str,
    status: &str,
    severity: remuda_protocol::Severity,
) -> Result<(), NodeError> {
    let payload = crate::DriverEmission::NativeLifecycle {
        name: name.to_owned(),
        status: status.to_owned(),
        severity,
    }
    .into_payload()?;
    store.append_observation(
        instance_id,
        None,
        remuda_protocol::Completeness::Structured,
        payload,
    )?;
    Ok(())
}

/// API socket for a Node's isolated session.
pub(crate) fn herdr_socket(config: &crate::NativeDriverConfig) -> PathBuf {
    config
        .herdr_socket_dir
        .as_ref()
        .map(|dir| dir.join("herdr.sock"))
        .unwrap_or_else(|| remuda_herdr::session_sockets(&config.herdr_session).api)
}

/// Whether the endpoint answers real work right now.
///
/// Probed with `session.snapshot` rather than `ping`: herdr keeps answering
/// `ping` while shutting down, which is precisely how a dying server used to
/// look healthy to the Node.
async fn carrier_usable(socket: &std::path::Path) -> bool {
    if !socket.exists() {
        return false;
    }
    remuda_herdr::Client::connect(socket)
        .with_timeout(std::time::Duration::from_secs(2))
        .session_snapshot()
        .await
        .is_ok()
}

/// True when a driver error means the Herdr carrier went away rather than the
/// agent refusing the work.
///
/// `remuda_driver::DriverError::CarrierUnavailable` is flattened to
/// `DriverError::Failed(String)` crossing the Node boundary, so this matches
/// the rendered text. The herdr wording is preserved verbatim by `map_herdr`,
/// which is what makes that safe.
pub(crate) fn is_carrier_loss(error: &crate::DriverError) -> bool {
    let text = error.to_string();
    text.contains("carrier unavailable")
        || text.contains(remuda_herdr::SERVER_UNAVAILABLE)
        || text.contains("server is shutting down")
        || text.contains("herdr unreachable")
}

/// Kick off one bounded recovery pass without blocking the caller.
///
/// Driver calls fail inside an Instance worker whose command must still be
/// settled promptly; recovery can take up to the policy's budget, so it runs
/// on its own task.
pub(crate) fn spawn_carrier_recovery(node: &DevNode) {
    let node = node.clone();
    tokio::spawn(async move {
        if let Err(error) = node.recover_herdr_carrier().await {
            tracing::error!(%error, "herdr carrier recovery failed");
        }
    });
}

/// Weak handle so an Instance worker can request recovery without keeping the
/// Node alive.
#[derive(Clone)]
pub(crate) struct CarrierSupervisor(std::sync::Weak<crate::runtime::DevNodeInner>);

impl CarrierSupervisor {
    pub(crate) fn new(inner: &Arc<crate::runtime::DevNodeInner>) -> Self {
        Self(Arc::downgrade(inner))
    }

    /// Recover if `error` looks like carrier loss. Returns whether a pass was
    /// started.
    pub(crate) fn on_driver_error(&self, error: &crate::DriverError) -> bool {
        if !is_carrier_loss(error) {
            return false;
        }
        let Some(inner) = self.0.upgrade() else {
            return false;
        };
        tracing::warn!(%error, "herdr carrier lost mid-session; attempting bounded recovery");
        spawn_carrier_recovery(&DevNode { inner });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DevServerConfig, DriverRegistry, MemoryStore, NativeDriverConfig};
    use remuda_herdr::{Client, WorkspaceCreateParams};
    use remuda_protocol::{DriverKind, HostId, WorkspaceId};
    use remuda_testing::{FakeHerdrOptions, FakeHerdrServer};
    use std::{path::Path, time::Duration};

    /// Short steps so the tests measure behaviour, not wall-clock patience.
    fn fast(max_wait: Duration) -> RetryPolicy {
        RetryPolicy::with_max_wait(max_wait)
            .with_backoff(Duration::from_millis(5), Duration::from_millis(20))
    }

    fn config(data_dir: &Path, socket_dir: &Path) -> NativeDriverConfig {
        let mut config = NativeDriverConfig::new(data_dir.to_path_buf());
        config.herdr_socket_dir = Some(socket_dir.to_path_buf());
        config.herdr_session = "remuda-node-carrier-test".into();
        config
    }

    fn node(store: Arc<MemoryStore>, config: NativeDriverConfig) -> DevNode {
        DevNode::with_parts(
            // The advertised workspace root stays the default "." — the crate
            // directory the harness runs in — so pin the allowlist there rather
            // than inheriting the $HOME default a /tmp checkout falls outside of.
            // The shared helper also covers the temp dir, so fixtures resolve.
            &DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
            store,
            DriverRegistry::with_fake().expect("fake registry"),
        )
        .expect("node")
        .with_herdr_config(config)
        .expect("herdr config")
    }

    fn ready_instance(store: &MemoryStore, id: InstanceId) {
        let instance = crate::runtime::fixture_instance(
            id,
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::GenericPty,
        )
        .expect("fixture instance");
        store.insert_instance(instance).expect("insert");
    }

    /// A Node-owned pane recorded in the store, as a real launch would.
    async fn owned_pane(client: &Client, store: &MemoryStore, instance_id: InstanceId) {
        let created = client
            .workspace_create(WorkspaceCreateParams {
                label: Some("remuda-carrier".into()),
                ..Default::default()
            })
            .await
            .expect("workspace");
        store
            .put_pty_resource(&remuda_driver::PtyResource {
                instance_id: Some(instance_id),
                socket_path: client.socket_path().into(),
                session: "remuda-node-carrier-test".into(),
                workspace_id: created.workspace.workspace_id,
                workspace_label: created.workspace.label,
                tab_id: created.tab.tab_id,
                pane_id: created.root_pane.pane_id,
            })
            .expect("record ownership");
    }

    fn journal_text(store: &dyn LocalStore, id: &InstanceId) -> String {
        let instance = store.get_instance(id).expect("instance");
        let page = store
            .read_events(&instance.journal_id, None, 512)
            .expect("journal");
        format!("{:?}", page.events)
    }

    /// The reported defect, at the layer it was fatal: a Node restarted while
    /// the previous Node's server is still shutting down on the same socket.
    ///
    /// Before the fix `reconcile_herdr` returned
    /// `driver error: herdr session.snapshot: server_unavailable: …` and
    /// `remuda dev` exited on the `?`.
    #[tokio::test]
    async fn startup_reconcile_survives_a_shutting_down_predecessor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_dir = dir.path().join("herdr");
        std::fs::create_dir_all(&socket_dir).expect("socket dir");
        let socket = socket_dir.join("herdr.sock");
        let client = Client::connect(&socket);
        let store = Arc::new(MemoryStore::new(64));
        let id = InstanceId::new();
        ready_instance(&store, id.clone());

        // A healthy server first, so there is real ownership to reconcile;
        // then the same socket held by a server that never finishes exiting.
        {
            let _healthy =
                FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).expect("fake herdr");
            owned_pane(&client, &store, id.clone()).await;
        }
        let _dying =
            FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket).shutting_down_for(usize::MAX))
                .expect("dying fake herdr");

        let node = node(store.clone(), config(dir.path(), &socket_dir));
        node.reconcile_herdr_with(fast(Duration::from_millis(120)))
            .await
            .expect("reconciliation must not fail the Node");

        assert_eq!(
            store.get_instance(&id).expect("instance").lifecycle,
            InstanceLifecycle::Exited,
        );
        assert!(
            store.pty_resources().expect("resources").is_empty(),
            "ownership of a pane on a dead server must not survive"
        );
    }

    /// Mid-session loss: restart the carrier, retire the Instance with a
    /// diagnostic, and never take the Node down.
    #[tokio::test]
    async fn mid_session_carrier_loss_recovers_and_marks_the_instance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_dir = dir.path().join("herdr");
        std::fs::create_dir_all(&socket_dir).expect("socket dir");
        let socket = socket_dir.join("herdr.sock");
        let client = Client::connect(&socket);
        let store = Arc::new(MemoryStore::new(64));
        let id = InstanceId::new();
        ready_instance(&store, id.clone());

        let fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).expect("fake herdr");
        owned_pane(&client, &store, id.clone()).await;

        let mut config = config(dir.path(), &socket_dir);
        // The replacement is the fake, so no real herdr is ever exec'd.
        config.herdr_binary = Some(remuda_testing::fake_herdr_bin());
        let node = node(store.clone(), config);

        // Kill the carrier out from under the live Instance.
        fake.shutdown().expect("stop fake herdr");

        let recovery = node
            .recover_herdr_carrier_with(fast(Duration::from_secs(10)))
            .await
            .expect("recovery must not fail the Node");
        assert!(recovery.restarted, "the session server should be back");
        assert_eq!(recovery.affected, vec![id.clone()]);

        Client::connect(&socket)
            .with_timeout(Duration::from_secs(2))
            .session_snapshot()
            .await
            .expect("restarted carrier serves real requests");
        // The replacement must be the executable the config pinned. A real
        // `herdr` here would mean recovery ignored `herdr_binary` and leaked a
        // long-lived server onto the developer's machine.
        assert!(
            !socket_dir.join("herdr").join("sessions").exists(),
            "only the real herdr writes a sessions/ tree; the fake must have been used"
        );

        assert_eq!(
            store.get_instance(&id).expect("instance").lifecycle,
            InstanceLifecycle::Failed,
        );
        let journal = journal_text(store.as_ref(), &id);
        assert!(
            journal.contains(CARRIER_LOST),
            "journal must explain the carrier loss: {journal}"
        );
        assert!(
            journal.contains(CARRIER_RESTARTED),
            "journal must record the restart: {journal}"
        );
    }

    /// A healthy carrier must never be mistaken for a lost one.
    #[tokio::test]
    async fn recovery_is_a_no_op_while_the_carrier_is_healthy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_dir = dir.path().join("herdr");
        std::fs::create_dir_all(&socket_dir).expect("socket dir");
        let socket = socket_dir.join("herdr.sock");
        let client = Client::connect(&socket);
        let store = Arc::new(MemoryStore::new(64));
        let id = InstanceId::new();
        ready_instance(&store, id.clone());
        let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).expect("fake herdr");
        owned_pane(&client, &store, id.clone()).await;

        let node = node(store.clone(), config(dir.path(), &socket_dir));
        let recovery = node
            .recover_herdr_carrier_with(fast(Duration::from_secs(2)))
            .await
            .expect("no-op recovery");
        assert!(!recovery.restarted, "nothing needed restarting");
        assert!(recovery.affected.is_empty(), "no Instance was affected");
        assert_eq!(
            store.get_instance(&id).expect("instance").lifecycle,
            InstanceLifecycle::Ready,
            "a healthy carrier must not retire a live Instance"
        );
        assert_eq!(
            store.pty_resources().expect("resources").len(),
            1,
            "live ownership must survive a no-op recovery"
        );
    }

    /// Only carrier loss triggers recovery; an ordinary driver failure must not.
    #[test]
    fn carrier_loss_is_distinguished_from_an_ordinary_driver_error() {
        assert!(is_carrier_loss(&crate::DriverError::Failed(
            "carrier unavailable: herdr session.snapshot: server_unavailable: server is shutting down"
                .into()
        )));
        assert!(is_carrier_loss(&crate::DriverError::Failed(
            "herdr unreachable (/tmp/herdr.sock)".into()
        )));
        assert!(!is_carrier_loss(&crate::DriverError::ControlUnavailable));
        assert!(!is_carrier_loss(&crate::DriverError::Failed(
            "agent refused the prompt".into()
        )));
    }
}
