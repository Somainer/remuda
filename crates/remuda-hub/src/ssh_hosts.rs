//! Hub-owned SSH supervision. The trusted carrier reuses the Node dispatcher;
//! no Hub bootstrap credential is sent to the remote shell or stored there.

use crate::{
    AppState, HubError,
    auth::{require_device, require_origin},
    config::{new_id, now_rfc3339},
    store::{HostRecord, Store, StoreError, load_host},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, post},
};
use remuda_ssh::{Backoff, BinaryPolicy, NodeTransport, SshClient, SshOptions, StdioTransport};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};

/// Local operator settings; API callers cannot choose executable paths or SSH options.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SshHostOptions {
    pub ssh_binary: PathBuf,
    pub upload_binary: Option<PathBuf>,
}

impl Default for SshHostOptions {
    fn default() -> Self {
        Self {
            ssh_binary: std::env::var_os("REMUDA_SSH_BINARY")
                .map(PathBuf::from)
                .unwrap_or_else(|| "ssh".into()),
            upload_binary: std::env::var_os("REMUDA_SSH_UPLOAD_BINARY").map(PathBuf::from),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddSshHost {
    target: String,
    label: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default, alias = "remudaBinaryPolicy")]
    remuda_binary_policy: BinaryPolicy,
}

#[derive(Clone)]
struct ManagedHost {
    id: String,
    target: String,
    policy: BinaryPolicy,
}

#[derive(Clone)]
struct SshState {
    hub: AppState,
    supervisor: Arc<Supervisor>,
}

pub(crate) fn routes(hub: AppState) -> Router<AppState> {
    let supervisor = Supervisor::new(hub.clone());
    Router::new()
        .route("/v1/hosts/ssh", post(add_host))
        .route("/v1/hosts/{id}", delete(remove_host))
        .with_state(SshState { hub, supervisor })
}

/// Owned only by the SSH routes. Tasks hold AppState without the supervisor,
/// so dropping the server (including a failed bind) cancels every SSH child.
#[derive(Default)]
struct Supervisor {
    tasks: Mutex<HashMap<String, JoinHandle<()>>>,
    restore: OnceLock<JoinHandle<()>>,
}

impl Supervisor {
    fn new(state: AppState) -> Arc<Self> {
        let supervisor = Arc::new(Self::default());
        let weak = Arc::downgrade(&supervisor);
        supervisor.restore.get_or_init(|| {
            tokio::spawn(async move {
                // Do not retain the router's owner across an await: otherwise
                // the restore task would prevent cancellation on server drop.
                let hosts = match state.store.managed_hosts().await {
                    Ok(hosts) => hosts,
                    Err(error) => {
                        tracing::error!(%error, "SSH host restore failed");
                        return;
                    }
                };
                if let Some(supervisor) = weak.upgrade() {
                    for host in hosts {
                        if let Err(error) = supervisor.start(state.clone(), host) {
                            tracing::error!(%error, "SSH supervisor start failed");
                        }
                    }
                }
            })
        });
        supervisor
    }

    fn start(&self, state: AppState, host: ManagedHost) -> Result<(), HubError> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| HubError::Internal("SSH supervisor lock poisoned".into()))?;
        let id = host.id.clone();
        if tasks.contains_key(&id) {
            return Ok(());
        }
        tasks.insert(id, tokio::spawn(supervise(state, host)));
        Ok(())
    }

    async fn stop(&self, id: &str) {
        let task = self
            .tasks
            .lock()
            .ok()
            .and_then(|mut tasks| tasks.remove(id));
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.get() {
            restore.abort();
        }
        if let Ok(tasks) = self.tasks.get_mut() {
            for (_, task) in tasks.drain() {
                task.abort();
            }
        }
    }
}

async fn add_host(
    State(SshState {
        hub: state,
        supervisor,
    }): State<SshState>,
    headers: HeaderMap,
    Json(mut body): Json<AddSshHost>,
) -> Result<(StatusCode, Json<Value>), HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    body.target = body.target.trim().into();
    body.label = body.label.trim().into();
    remuda_ssh::validate_target(&body.target)
        .map_err(|error| HubError::BadRequest(error.to_string()))?;
    if body.label.is_empty()
        || body.label.len() > 128
        || body.label.chars().any(char::is_control)
        || body.labels.len() > 32
        || body.labels.iter().any(|label| {
            let label = label.trim();
            label.is_empty()
                || label.len() > 128
                || label.chars().any(char::is_control)
                || label
                    .split_once('=')
                    .or_else(|| label.split_once(':'))
                    .is_some_and(|(key, value)| key.trim().is_empty() || value.trim().is_empty())
        })
    {
        return Err(HubError::BadRequest(
            "label must be 1–128 characters; at most 32 nonempty labels of 128 characters".into(),
        ));
    }
    // Canonicalize both egress:gateway and egress=gateway for Node inventory.
    body.labels = body
        .labels
        .iter()
        .map(|label| label.trim().replacen(':', "=", 1))
        .collect();
    let id = new_id("hst").map_err(|e| HubError::Internal(e.to_string()))?;
    let managed = ManagedHost {
        id: id.clone(),
        target: body.target.clone(),
        policy: body.remuda_binary_policy,
    };
    let host = state.store.insert_managed_host(id, body).await?;
    supervisor.start(state.clone(), managed)?;
    Ok((StatusCode::CREATED, Json(crate::registry::host_view(&host))))
}

async fn remove_host(
    State(SshState {
        hub: state,
        supervisor,
    }): State<SshState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    // Retire atomically before cancelling; this fences concurrent placement and hello.
    state.store.retire_managed_host(id.clone()).await?;
    supervisor.stop(&id).await;
    // D-048: revoke/tear down relay streams before removing the live link.
    state.api_relay.on_link_lost(&state, &id).await;
    state.nodes.remove(&id).await;
    state.store.mark_host_offline(id.clone()).await?;
    state
        .store
        .run_named("remove_host", move |conn| {
            conn.execute("DELETE FROM hosts WHERE id = ?1", [id])?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

impl Store {
    /// Rotate an in-memory credential for this Hub-owned carrier. Only its hash
    /// is stored; neither this credential nor the bootstrap token goes to SSH.
    async fn managed_session_token(&self, id: String) -> Result<String, HubError> {
        let token = crate::config::random_token();
        let hash = crate::auth::hash_secret(&token)?;
        let prefix = crate::auth::token_prefix(&token).map(str::to_owned);
        let changed = self
            .run_named("managed_session_token", move |conn| {
                Ok(conn.execute(
                    "UPDATE hosts SET token_hash = ?2, token_prefix = ?3 WHERE id = ?1 AND state != 'retired'
                     AND EXISTS (SELECT 1 FROM ssh_hosts WHERE host_id = hosts.id)",
                    params![id, hash, prefix],
                )?)
            })
            .await?;
        if changed != 1 {
            return Err(HubError::Unauthenticated);
        }
        Ok(token)
    }

    async fn insert_managed_host(
        &self,
        id: String,
        body: AddSshHost,
    ) -> Result<HostRecord, HubError> {
        let result = self.run_named("insert_managed_host", move |conn| {
            let tx = conn.transaction()?;
            if tx.query_row("SELECT 1 FROM ssh_hosts WHERE target = ?1", [&body.target], |_| Ok(())).optional()?.is_some() {
                return Err(StoreError::Id("SSH target already registered".into()));
            }
            tx.execute("INSERT INTO hosts (id, label, token_hash, state, created_at, transport, labels_json) VALUES (?1, ?2, '', 'connecting', ?3, 'ssh-stdio', ?4)",
                params![id, body.label, now_rfc3339(), serde_json::to_string(&body.labels)?])?;
            tx.execute("INSERT INTO ssh_hosts (host_id, target, policy_json) VALUES (?1, ?2, ?3)", params![id, body.target, serde_json::to_string(&body.remuda_binary_policy)?])?;
            tx.commit()?;
            load_host(conn, &id)?.ok_or_else(|| StoreError::Id("SSH host insert missing".into()))
        }).await;
        match result {
            Err(StoreError::Id(message)) if message == "SSH target already registered" => {
                Err(HubError::Conflict(message))
            }
            other => other.map_err(Into::into),
        }
    }

    async fn managed_hosts(&self) -> Result<Vec<ManagedHost>, StoreError> {
        self.run_named("managed_hosts", |conn| {
            let mut stmt = conn.prepare("SELECT host_id, target, policy_json FROM ssh_hosts JOIN hosts ON hosts.id = ssh_hosts.host_id WHERE state != 'retired'")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?;
            rows.map(|row| { let (id, target, policy) = row?; Ok(ManagedHost { id, target, policy: serde_json::from_str(&policy)? }) }).collect()
        }).await
    }

    async fn ssh_status(
        &self,
        id: String,
        status: &str,
        error: Option<String>,
    ) -> Result<(), StoreError> {
        let status = status.to_owned();
        self.run_named("ssh_status", move |conn| {
            conn.execute(
                "UPDATE hosts SET state = ?2 WHERE id = ?1 AND state != 'retired'",
                params![id, status],
            )?;
            conn.execute(
                "UPDATE ssh_hosts SET last_error = ?2 WHERE host_id = ?1",
                params![id, error],
            )?;
            Ok(())
        })
        .await
    }

    /// A bridge is only a controller link. Begin host-loss grace after a failed
    /// independent daemon probe, and never reset that timer on another retry.
    async fn ssh_daemon_probe(
        &self,
        id: String,
        reachable: bool,
        error: Option<String>,
    ) -> Result<(), StoreError> {
        self.run_named("ssh_daemon_probe", move |conn| {
            conn.execute(
                "UPDATE hosts SET state = ?2, offline_since = CASE WHEN ?3 THEN NULL
                 WHEN state = 'daemon-unreachable' THEN COALESCE(offline_since, ?4)
                 ELSE ?4 END WHERE id = ?1 AND state != 'retired'",
                params![
                    id,
                    if reachable {
                        "offline-alive"
                    } else {
                        "daemon-unreachable"
                    },
                    reachable,
                    now_rfc3339()
                ],
            )?;
            conn.execute(
                "UPDATE ssh_hosts SET last_error = ?2 WHERE host_id = ?1",
                params![id, error],
            )?;
            Ok(())
        })
        .await
    }

    /// Reconcile observed state without trusting the Node to advance the Hub's
    /// durable journal watermark. Replay is still required for every missing seq.
    pub(crate) async fn reconcile_daemon_instances(
        &self,
        host_id: String,
        instances: Vec<Value>,
    ) -> Result<(), StoreError> {
        self.run_named("reconcile_daemon_instances", move |conn| {
            let tx = conn.transaction()?;
            for instance in instances {
                if instance["hostId"].as_str() != Some(&host_id) { continue; }
                let Some(id) = instance["id"].as_str().or_else(|| instance["instanceId"].as_str()) else { continue; };
                let lifecycle = match instance["lifecycle"].as_str() {
                    Some("ready" | "running") => "running",
                    Some(value @ ("requested" | "preparing" | "starting" | "closing" | "exited" | "failed" | "unknown" | "reconciling")) => value,
                    _ => continue,
                };
                let activity = instance["activity"].as_str().or_else(|| instance["activity"]["value"].as_str());
                let activity = match activity {
                    Some("waiting-interaction") => "blocked",
                    Some(value @ ("idle" | "working" | "blocked" | "draining")) => value,
                    _ => "unknown",
                };
                tx.execute(
                    "UPDATE instances SET lifecycle = ?3, activity = ?4, connectivity = 'connected',
                     last_error = ?5, updated_at = ?6 WHERE id = ?1 AND host_id = ?2",
                    params![id, host_id, lifecycle, activity, instance["lastError"].as_str(), now_rfc3339()],
                )?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    async fn retire_managed_host(&self, id: String) -> Result<(), HubError> {
        let outcome = self.run_named("retire_managed_host", move |conn| {
            let tx = conn.transaction()?;
            if tx.query_row("SELECT 1 FROM ssh_hosts WHERE host_id = ?1", [&id], |_| Ok(())).optional()?.is_none() { return Ok(false); }
            let active: i64 = tx.query_row("SELECT COUNT(*) FROM instances WHERE host_id = ?1 AND lifecycle NOT IN ('exited', 'failed', 'archived')", [&id], |row| row.get(0))?;
            if active > 0 { return Err(StoreError::Id("stop active instances before removing this SSH host".into())); }
            tx.execute("UPDATE hosts SET state = 'retired', token_hash = '', token_prefix = NULL WHERE id = ?1", [&id])?;
            tx.commit()?;
            Ok(true)
        }).await;
        match outcome {
            Ok(true) => Ok(()),
            Ok(false) => Err(HubError::NotFound),
            Err(StoreError::Id(message)) => Err(HubError::Conflict(message)),
            Err(error) => Err(error.into()),
        }
    }
}

async fn supervise(state: AppState, host: ManagedHost) {
    let client = SshClient::new(
        &host.target,
        SshOptions {
            ssh_binary: state.config.ssh_hosts.ssh_binary.clone(),
            ..SshOptions::default()
        },
    );
    let mut attempt = 0u32;
    loop {
        let Some(record) = state.store.get_host(host.id.clone()).await.ok().flatten() else {
            return;
        };
        if record.state == "retired" {
            return;
        }
        if !matches!(
            record.state.as_str(),
            "offline-alive" | "daemon-unreachable"
        ) {
            let _ = state
                .store
                .ssh_status(host.id.clone(), "connecting", record.last_error.clone())
                .await;
        }
        let started = Instant::now();
        let mut generation = None;
        let result = connect_once(&state, &host, &record, &client, &mut generation).await;
        if let Some(generation) = generation
            && !state.nodes.remove_generation(&host.id, generation).await
        {
            return;
        }
        // D-048: tear down relay streams this SSH carrier owned, exactly as
        // the WebSocket session teardown does — otherwise proxy-link losses
        // never reach W, worker blocks never fire, and stream slots leak.
        state.api_relay.on_link_lost(&state, &host.id).await;
        let _ = state.store.mark_host_offline(host.id.clone()).await;
        let message = match result {
            Ok(()) => "SSH connection closed".into(),
            Err(error) => error.to_string(),
        };
        let reachable = remuda_ssh::ManagedNode::daemon_reachable(&client, &host.id)
            .await
            .unwrap_or(false);
        let _ = state
            .store
            .ssh_daemon_probe(
                host.id.clone(),
                reachable,
                Some(message.chars().take(1000).collect()),
            )
            .await;
        if started.elapsed() > Duration::from_secs(60) {
            attempt = 0;
        }
        tokio::time::sleep(Backoff::default().delay(attempt)).await;
        attempt = attempt.saturating_add(1).min(8);
    }
}

async fn connect_once(
    state: &AppState,
    managed: &ManagedHost,
    record: &HostRecord,
    client: &SshClient,
    generation: &mut Option<u64>,
) -> anyhow::Result<()> {
    // Also bounds a stalled binary upload before the SSH child's output wait.
    let prepared = tokio::time::timeout(
        Duration::from_secs(180),
        remuda_ssh::prepare_managed_node(
            client,
            &managed.id,
            &record.label,
            &record.labels,
            managed.policy,
            state.config.ssh_hosts.upload_binary.as_deref(),
        ),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("SSH preflight timed out after 180s; check SSH and the upload artifact")
    })??;
    state
        .store
        .ssh_daemon_probe(managed.id.clone(), true, record.last_error.clone())
        .await?;
    let mut carrier = StdioTransport::connect_ssh(client.clone(), prepared.argv).await?;
    let hello = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let frame = carrier.recv_json().await?.ok_or_else(|| {
                anyhow::anyhow!(
                    "SSH Node exited before hello; check its binary and CLI dependencies"
                )
            })?;
            if frame["method"] == "node.auth" {
                continue;
            }
            return Ok::<_, anyhow::Error>(frame);
        }
    })
    .await??;
    anyhow::ensure!(
        hello["method"] == "node.hello"
            && hello["params"]["hostId"] == managed.id
            && hello["params"]["bridge"] == true
            && hello["params"]["daemon"] == true
            && hello.get("id").is_some(),
        "SSH Node hello identity/protocol mismatch; persistent daemon bridge required"
    );
    bridge_stdio_session(state, managed, record, &mut carrier, hello, generation).await
}

/// Run one persistent-daemon bridge session over a connected stdio carrier
/// whose `node.hello` frame has already been received (but not answered).
///
/// Generic over [`NodeTransport`] so the in-process test harness can drive
/// the exact post-hello production path — hello dispatch, reply, and the
/// shared post-hello reconciliation — without an SSH daemon.
async fn bridge_stdio_session<T: NodeTransport>(
    state: &AppState,
    managed: &ManagedHost,
    record: &HostRecord,
    carrier: &mut T,
    hello: Value,
    generation: &mut Option<u64>,
) -> anyhow::Result<()> {
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(32);
    let pending = crate::transport::new_pending_rpcs();
    let mut host_id = None;
    let mut hello_done = false;
    let mut params = hello["params"].clone();
    params["label"] = json!(record.label);
    params["transport"] = json!("ssh-stdio");
    let session_token = state
        .store
        .managed_session_token(managed.id.clone())
        .await?;
    let result = crate::ws::handle_node_method(
        state,
        &session_token,
        &mut host_id,
        &mut hello_done,
        generation,
        "node.hello",
        params,
        &out_tx,
        &pending,
    )
    .await?;
    state
        .store
        .reconcile_daemon_instances(
            managed.id.clone(),
            hello["params"]["instances"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        )
        .await?;
    carrier
        .send_json(&crate::error::rpc_ok(
            hello["id"].clone(),
            result.unwrap_or(Value::Null),
        ))
        .await?;
    // The hello reply is on the wire now. Run the SAME post-hello
    // reconciliation as the outbound WSS carrier (D-048 egress re-push and
    // the c-dirpicker unregister-abort sweep, r9 item 1): without the sweep
    // an SSH-managed daemon keeps a stale unbinding mark until ITS process
    // restarts. Both spawn after the reply so their frames cannot overtake
    // it on the bounded out_rx channel.
    crate::ws::spawn_post_hello_tasks(state, managed.id.clone());
    state
        .store
        .ssh_status(managed.id.clone(), "online", None)
        .await?;
    loop {
        tokio::select! {
            outgoing = out_rx.recv() => {
                let Some(frame) = outgoing else { break; };
                tokio::time::timeout(Duration::from_secs(15), carrier.send_json(&frame)).await??;
            }
            incoming = carrier.recv_json() => {
                let Some(frame) = incoming? else { break; };
                if frame.get("method").is_none() {
                    if let Some(id) = frame["id"].as_str()
                        && let Some(call) = pending
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(id)
                    {
                        let _ = call.tx.send(frame);
                    }
                    continue;
                }
                let method = frame["method"].as_str().unwrap_or("");
                let result = crate::ws::handle_node_method(state, &session_token, &mut host_id, &mut hello_done, generation, method, frame["params"].clone(), &out_tx, &pending).await;
                if let Some(id) = frame.get("id").filter(|id| !id.is_null()) {
                    let reply = match result { Ok(Some(value)) => crate::error::rpc_ok(id.clone(), value), Ok(None) => continue, Err(error) => crate::error::rpc_error(id.clone(), -32000, &error.to_string()) };
                    tokio::time::timeout(Duration::from_secs(15), carrier.send_json(&reply)).await??;
                }
            }
        }
    }
    // Stop waiters hanging until their timeout once the stdio carrier is gone.
    crate::transport::fail_all_pending(&pending);
    carrier.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bridge_loss_grace_begins_only_after_daemon_probe_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let id = new_id("hst").unwrap();
        store
            .insert_managed_host(
                id.clone(),
                AddSshHost {
                    target: "test-node".into(),
                    label: "SSH test".into(),
                    labels: vec![],
                    remuda_binary_policy: BinaryPolicy::RequireInstalled,
                },
            )
            .await
            .unwrap();
        store.ssh_status(id.clone(), "online", None).await.unwrap();
        let instance = store
            .insert_instance(
                id.clone(),
                None,
                "claude".into(),
                "claude-print".into(),
                None,
                json!({}),
            )
            .await
            .unwrap();
        let host_id = id.clone();
        store
            .run_named(
                "bridge_loss_grace_begins_only_after_daemon_probe_fails",
                move |conn| {
                    conn.execute(
                        "UPDATE hosts SET last_seen_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
                        [host_id],
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        store.mark_all_hosts_offline().await.unwrap();
        assert_eq!(store.expire_lost_hosts(60_000).await.unwrap(), 0);
        store
            .run_named(
                "bridge_loss_grace_begins_only_after_daemon_probe_fails",
                move |conn| {
                    conn.execute(
                        "UPDATE hosts SET offline_since = '2000-01-01T00:00:00Z' WHERE id = ?1",
                        [id],
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(store.expire_lost_hosts(60_000).await.unwrap(), 0);
        let id = instance.host_id.clone();
        store
            .ssh_daemon_probe(id.clone(), true, None)
            .await
            .unwrap();
        assert_eq!(store.expire_lost_hosts(0).await.unwrap(), 0);
        let disconnected_id = id.clone();
        store
            .run_named("bridge_loss_grace_begins_only_after_daemon_probe_fails", move |conn| {
                conn.execute(
                    "UPDATE hosts SET state = 'offline', offline_since = '2000-01-01T00:00:00Z' WHERE id = ?1",
                    [disconnected_id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        store
            .ssh_daemon_probe(id.clone(), false, None)
            .await
            .unwrap();
        assert_eq!(
            store.expire_lost_hosts(60_000).await.unwrap(),
            0,
            "first failed daemon probe must reset the earlier bridge disconnect timestamp"
        );
        let old_id = id.clone();
        store
            .run_named(
                "bridge_loss_grace_begins_only_after_daemon_probe_fails",
                move |conn| {
                    conn.execute(
                        "UPDATE hosts SET offline_since = '2000-01-01T00:00:00Z' WHERE id = ?1",
                        [old_id],
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        store.ssh_daemon_probe(id, false, None).await.unwrap();
        assert_eq!(
            store.expire_lost_hosts(60_000).await.unwrap(),
            1,
            "failed retries must not restart grace"
        );
        let instance = store
            .get_instance(instance.instance_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instance.lifecycle, "exited");
        assert_eq!(instance.last_error.as_deref(), Some("host-lost"));
        store.close().await;
    }

    #[tokio::test]
    async fn daemon_snapshot_is_host_scoped_and_cannot_advance_acked_seq() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut instances = Vec::new();
        for target in ["first-node", "other-node"] {
            let host = new_id("hst").unwrap();
            store
                .insert_managed_host(
                    host.clone(),
                    AddSshHost {
                        target: target.into(),
                        label: target.into(),
                        labels: vec![],
                        remuda_binary_policy: BinaryPolicy::RequireInstalled,
                    },
                )
                .await
                .unwrap();
            store
                .ssh_status(host.clone(), "online", None)
                .await
                .unwrap();
            instances.push(
                store
                    .insert_instance(
                        host,
                        None,
                        "codex".into(),
                        "codex-appserver".into(),
                        None,
                        json!({}),
                    )
                    .await
                    .unwrap(),
            );
        }
        let own = &instances[0];
        let other = &instances[1];
        store
            .append_journal(
                own.host_id.clone(),
                own.instance_id.clone(),
                Some(1),
                json!({"kind":"lifecycle","payload":{"type":"entity","state":"ready"}}),
            )
            .await
            .unwrap();
        store.reconcile_daemon_instances(own.host_id.clone(), vec![
            json!({"id":own.instance_id,"hostId":own.host_id,"lifecycle":"exited","activity":{"state":"known","value":"idle"},"durableSeq":"999"}),
            json!({"id":other.instance_id,"hostId":own.host_id,"lifecycle":"exited","activity":"idle"}),
        ]).await.unwrap();
        let updated = store
            .get_instance(own.instance_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.lifecycle, "exited");
        assert_eq!(updated.activity, "idle");
        assert_eq!(updated.durable_seq, "1");
        let untouched = store
            .get_instance(other.instance_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.lifecycle, other.lifecycle);
        store.close().await;
    }

    /// In-process stand-in for the SSH child's NDJSON carrier: frames the
    /// bridge writes land on a channel a scripted Node task reads, and frames
    /// that task pushes are what the bridge receives. `shutdown` models the
    /// stdio child closing (clean `Ok(None)`).
    struct ScriptedStdio {
        to_node: mpsc::UnboundedSender<Value>,
        from_node: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Value>>>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    }

    impl NodeTransport for ScriptedStdio {
        async fn send_json(&mut self, value: &Value) -> std::result::Result<(), remuda_ssh::Error> {
            self.to_node
                .send(value.clone())
                .map_err(|_| remuda_ssh::Error::Disconnected)
        }

        async fn recv_json(&mut self) -> std::result::Result<Option<Value>, remuda_ssh::Error> {
            let mut rx = self.from_node.lock().await;
            tokio::select! {
                frame = rx.recv() => Ok(frame),
                _ = self.shutdown.wait_for(|stop| *stop) => Ok(None),
            }
        }

        async fn close(&mut self) -> std::result::Result<(), remuda_ssh::Error> {
            Ok(())
        }
    }

    /// c-dirpicker r9 item 1: the SSH-stdio carrier must run the SAME
    /// post-hello unregister abort sweep as the outbound WSS carrier. Drives
    /// the real `bridge_stdio_session` post-hello path with an unsettled
    /// unregister left by a dead previous link and asserts the abort frame
    /// reaches the Node over the stdio bridge, the Hub settles the stuck
    /// command, and a following `instance.create` succeeds.
    #[tokio::test]
    async fn ssh_stdio_hello_aborts_an_unsettled_unregister_and_unblocks_create() {
        use crate::HubConfig;

        let dir = tempfile::tempdir().unwrap();
        let hub = crate::spawn(HubConfig::for_test(dir.path().join("data")))
            .await
            .unwrap();
        let state = hub.state.clone();
        let store = state.store.clone();

        let host = new_id("hst").unwrap();
        store
            .insert_managed_host(
                host.clone(),
                AddSshHost {
                    target: "test-node".into(),
                    label: "SSH test".into(),
                    labels: vec![],
                    remuda_binary_policy: BinaryPolicy::RequireInstalled,
                },
            )
            .await
            .unwrap();
        let record = store
            .get_host(host.clone())
            .await
            .unwrap()
            .expect("managed host row");
        let managed = ManagedHost {
            id: host.clone(),
            target: "test-node".into(),
            policy: BinaryPolicy::RequireInstalled,
        };

        // The stale command a dropped previous link left behind: prepare is
        // assumed to have landed (the Node would hold the unbinding mark),
        // commit never did.
        const WSP: &str = "wsp_01993ab0-0000-7000-8000-0000000000d9";
        const ROOT: &str = "/srv/remuda-dp-r9/proj";
        let (command, _) = store
            .queue_command(
                None,
                None,
                host.clone(),
                "workspace.unregister".into(),
                json!({"path": ROOT, "workspaceId": WSP}),
                None,
            )
            .await
            .unwrap();

        let (to_node_tx, mut to_node_rx) = mpsc::unbounded_channel::<Value>();
        let (from_node_tx, from_node_rx) = mpsc::unbounded_channel::<Value>();
        let from_node = Arc::new(tokio::sync::Mutex::new(from_node_rx));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let hello = json!({
            "jsonrpc": "2.0", "id": "ssh-hello", "method": "node.hello",
            "params": {"hostId": host, "bridge": true, "daemon": true,
                       "instances": [], "nodeVersion": "0.1.0-dp-r9"}
        });

        let abort_seen = Arc::new(tokio::sync::Notify::new());
        let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        // The durable unbinding mark the dead previous link prepared: set
        // until the abort is PROCESSED. The abort answer is parked on the
        // `release_abort` watch so the test can observe a create colliding
        // with the mark BEFORE the abort path clears it.
        let unbinding_mark = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (release_abort_tx, release_abort_rx) = tokio::sync::watch::channel(false);

        // The scripted persistent daemon: hello first, then answer the
        // post-hello abort and the follow-up create over the SAME bridge.
        let node = {
            let abort_seen = abort_seen.clone();
            let captured = captured.clone();
            let unbinding_mark = unbinding_mark.clone();
            let hello = hello.clone();
            tokio::spawn(async move {
                from_node_tx.send(hello).unwrap();
                while let Some(frame) = to_node_rx.recv().await {
                    let id = frame.get("id").cloned().unwrap_or(Value::Null);
                    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
                    let params = frame.get("params").cloned().unwrap_or(json!({}));
                    match method {
                        "workspace.unregister" if params["phase"] == "abort" => {
                            captured.lock().unwrap().push(params.clone());
                            // The abort frame has arrived but, like the real
                            // Node, the mark is only released when the abort
                            // is processed and acked. Park the answer (the
                            // recv loop keeps draining, so a create still
                            // gets its conflict reply) until the test
                            // releases it.
                            abort_seen.notify_waiters();
                            let mut released = release_abort_rx.clone();
                            let reply_tx = from_node_tx.clone();
                            let unbinding_mark = unbinding_mark.clone();
                            tokio::spawn(async move {
                                let _ = released.wait_for(|stop| *stop).await;
                                unbinding_mark.store(false, std::sync::atomic::Ordering::SeqCst);
                                reply_tx
                                    .send(json!({
                                        "jsonrpc": "2.0", "id": id,
                                        "result": {
                                            "commandId": params["commandId"].clone(),
                                            "workspaceId": params["workspaceId"].clone(),
                                            "phase": "aborted",
                                        }
                                    }))
                                    .unwrap();
                            });
                        }
                        "instance.create" => {
                            // The real Node's create reservation
                            // (DevNode::reserve_locked): while the unbinding
                            // mark is set this is a Conflict, code -32009,
                            // the message the Hub surfaces as 409.
                            if unbinding_mark.load(std::sync::atomic::Ordering::SeqCst) {
                                from_node_tx
                                    .send(json!({
                                        "jsonrpc": "2.0", "id": id,
                                        "error": {
                                            "code": -32009,
                                            "message": format!(
                                                "workspace {ROOT} is being unregistered; \
                                                 wait for it to settle before starting a session"
                                            ),
                                        }
                                    }))
                                    .unwrap();
                            } else {
                                from_node_tx
                                    .send(json!({
                                        "jsonrpc": "2.0", "id": id,
                                        "result": {"ok": true, "instanceId": "it_dp_r10_create"}
                                    }))
                                    .unwrap();
                            }
                        }
                        // The hello reply (and anything else) needs no answer.
                        _ => {}
                    }
                }
            })
        };

        let mut carrier = ScriptedStdio {
            to_node: to_node_tx,
            from_node,
            shutdown: shutdown_rx,
        };
        let bridge = {
            let state = state.clone();
            let managed = managed.clone();
            let record = record.clone();
            tokio::spawn(async move {
                bridge_stdio_session(&state, &managed, &record, &mut carrier, hello, &mut None)
                    .await
            })
        };

        // The sweep (spawned after state.nodes.insert and the hello reply)
        // must deliver the abort on THIS stdio carrier.
        tokio::time::timeout(Duration::from_secs(10), abort_seen.notified())
            .await
            .expect("the unregister abort never arrived on the ssh stdio carrier");
        let seen = captured.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0]["phase"], json!("abort"));
        assert_eq!(seen[0]["path"], json!(ROOT));
        assert_eq!(seen[0]["workspaceId"], json!(WSP));
        assert_eq!(seen[0]["commandId"], json!(command.command_id));

        // r10 item 3: the create-after-abort assertion used to be vacuous —
        // the scripted Node answered every create OK, so it passed even if
        // the abort never ran. With the abort ANSWER parked and the unbinding
        // mark still held, the create must now hit the Node's conflict:
        // JSON-RPC -32009 carrying the 409 "being unregistered" message.
        let blocked = tokio::time::timeout(
            Duration::from_secs(10),
            state.nodes.call(
                &host,
                "instance.create",
                json!({"workspaceId": WSP}),
                Duration::from_secs(10),
            ),
        )
        .await
        .expect("pre-abort create timed out")
        .expect("the pre-abort create reached the node")
        .expect("the pre-abort create carried an RPC frame");
        assert_eq!(blocked["error"]["code"], json!(-32009), "{blocked}");
        let conflict_message = blocked["error"]["message"]
            .as_str()
            .expect("the conflict carries a message");
        assert!(
            conflict_message.contains("being unregistered"),
            "{conflict_message}"
        );
        assert!(conflict_message.contains(ROOT), "{conflict_message}");
        // The stuck command is still unsettled: the abort has not been
        // processed yet, so nothing could have settled it.
        assert_ne!(
            store
                .get_command(command.command_id.clone())
                .await
                .expect("command row")
                .expect("command row")
                .state,
            "settled"
        );

        // Let the Node process the abort: it clears the mark and acks.
        release_abort_tx.send(true).unwrap();

        // The Hub only settles the stuck command after the Node acked.
        let row = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = store
                    .get_command(command.command_id.clone())
                    .await
                    .unwrap()
                    .expect("command row");
                if row.state == "settled" {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the stuck unregister never settled");
        assert_eq!(row.settlement_outcome.as_deref(), Some("rejected"));

        // The wedge is gone ONLY because the abort ran: the SAME create that
        // just collided with the mark now succeeds on the same bridge.
        let answer = tokio::time::timeout(
            Duration::from_secs(10),
            crate::http::call_node(
                &state,
                &host,
                "instance.create",
                json!({"workspaceId": WSP}),
            ),
        )
        .await
        .expect("create timed out")
        .expect("a following create must succeed after the abort");
        assert_eq!(answer["instanceId"], json!("it_dp_r10_create"));

        // Close the carrier (child exit) and join the bridge and the node.
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), bridge)
            .await
            .expect("bridge join timed out")
            .expect("bridge task panicked")
            .expect("bridge ended in error");
        tokio::time::timeout(Duration::from_secs(10), node)
            .await
            .expect("scripted node join timed out")
            .unwrap();
        hub.shutdown().await;
    }
}
