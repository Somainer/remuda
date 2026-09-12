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
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc, oneshot},
    task::JoinHandle,
};

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
    state.nodes.remove(&id).await;
    state.store.mark_host_offline(id.clone()).await?;
    state
        .store
        .run(move |conn| {
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
            .run(move |conn| {
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
        let result = self.run(move |conn| {
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
        self.run(|conn| {
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
        self.run(move |conn| {
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

    async fn retire_managed_host(&self, id: String) -> Result<(), HubError> {
        let outcome = self.run(move |conn| {
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
        let _ = state
            .store
            .ssh_status(host.id.clone(), "connecting", record.last_error.clone())
            .await;
        let started = Instant::now();
        let result = connect_once(&state, &host, &record, &client).await;
        state.nodes.remove(&host.id).await;
        let _ = state.store.mark_host_offline(host.id.clone()).await;
        let message = match result {
            Ok(()) => "SSH connection closed".into(),
            Err(error) => error.to_string(),
        };
        let _ = state
            .store
            .ssh_status(
                host.id.clone(),
                "offline",
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
            && hello.get("id").is_some(),
        "SSH Node hello identity/protocol mismatch"
    );
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(32);
    let pending = Arc::new(AsyncMutex::new(
        HashMap::<String, oneshot::Sender<Value>>::new(),
    ));
    let mut host_id = None;
    let mut hello_done = false;
    let mut generation = None;
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
        &mut generation,
        "node.hello",
        params,
        &out_tx,
        &pending,
    )
    .await?;
    carrier
        .send_json(&crate::error::rpc_ok(
            hello["id"].clone(),
            result.unwrap_or(Value::Null),
        ))
        .await?;
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
                    if let Some(id) = frame["id"].as_str() && let Some(tx) = pending.lock().await.remove(id) { let _ = tx.send(frame); }
                    continue;
                }
                let method = frame["method"].as_str().unwrap_or("");
                let result = crate::ws::handle_node_method(state, &session_token, &mut host_id, &mut hello_done, &mut generation, method, frame["params"].clone(), &out_tx, &pending).await;
                if let Some(id) = frame.get("id").filter(|id| !id.is_null()) {
                    let reply = match result { Ok(Some(value)) => crate::error::rpc_ok(id.clone(), value), Ok(None) => continue, Err(error) => crate::error::rpc_error(id.clone(), -32000, &error.to_string()) };
                    carrier.send_json(&reply).await?;
                }
            }
        }
    }
    carrier.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restart_grace_uses_ssh_disconnect_time_instead_of_old_inventory() {
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
            .run(move |conn| {
                conn.execute(
                    "UPDATE hosts SET last_seen_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
                    [host_id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        store.mark_all_hosts_offline().await.unwrap();
        assert_eq!(store.expire_lost_hosts(60_000).await.unwrap(), 0);
        store
            .run(move |conn| {
                conn.execute(
                    "UPDATE hosts SET offline_since = '2000-01-01T00:00:00Z' WHERE id = ?1",
                    [id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(store.expire_lost_hosts(60_000).await.unwrap(), 1);
        let instance = store
            .get_instance(instance.instance_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instance.lifecycle, "exited");
        assert_eq!(instance.last_error.as_deref(), Some("host-lost"));
        store.close().await;
    }
}
