//! Single-writer SQLite actor. Connections never cross `.await`.

use crate::config::{new_id, now_rfc3339};
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::oneshot;

#[cfg(test)]
#[path = "store_auth_tests.rs"]
mod auth_tests;

/// SQLite or actor failures.
#[derive(Debug, Error)]
pub enum StoreError {
    /// rusqlite.
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON column.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Writer thread exited.
    #[error("store closed")]
    Closed,
    /// Protocol ID.
    #[error("id: {0}")]
    Id(String),
}

fn sqlite_is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

const BUSY_WAIT: Duration = Duration::from_secs(5);

enum Job {
    Run(Box<dyn FnOnce(&mut Connection) + Send>),
    Stop(std::sync::mpsc::Sender<()>),
}

struct StoreJoin {
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Drop for StoreJoin {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.thread.lock()
            && let Some(thread) = guard.take()
        {
            let _ = thread.join();
        }
    }
}

/// Handle to the Hub database writer.
#[derive(Clone)]
pub struct Store {
    tx: std::sync::mpsc::Sender<Job>,
    /// Joins the writer thread after the last clone drops its channel sender.
    _join: Arc<StoreJoin>,
}

/// Result of presenting a Node bootstrap or host token.
pub enum HostAuthOutcome {
    /// Token matched an enrolled host, or bootstrap enrolled a new one.
    Authenticated {
        /// Host index row.
        host: Box<HostRecord>,
        /// Fresh host token, only on first enrollment.
        node_token: Option<String>,
    },
    /// Secret did not match bootstrap or any host verifier.
    Rejected,
}

/// Inputs for [`Store::authenticate_host`].
pub struct HostAuthRequest {
    /// Presented bearer secret.
    pub presented: String,
    /// Configured bootstrap token.
    pub bootstrap: String,
    /// Optional `hostId` from hello params.
    pub hello_host_id: Option<String>,
    /// Optional label.
    pub label: Option<String>,
    /// Optional node version.
    pub node_version: Option<String>,
}

/// Device row.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    /// `dev_…`.
    pub id: String,
    /// Caller-supplied label.
    pub name: String,
}

/// Host index row (Hub projection).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRecord {
    /// `hst_…`.
    pub host_id: String,
    /// Display name.
    pub label: String,
    /// Protocol host state.
    pub state: String,
    /// Convenience projection of `state == online`.
    pub online: bool,
    /// Last heartbeat or hello.
    pub last_seen_at: Option<String>,
    /// Node binary version.
    pub node_version: Option<String>,
    /// CLI inventory (`kind` / `version` / `path` / `auth`).
    pub cli: Value,
    /// Opaque capability snapshot from `host.report`.
    pub capabilities: Value,
    /// Instance count on this host.
    pub instance_count: i64,
    /// Transport mode (`outbound-wss` / `ssh-stdio`).
    pub transport: String,
    /// Placement tags (`region=sg`).
    pub labels: Vec<String>,
    /// Herdr binary/socket when advertised.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub herdr: Option<Value>,
    /// Load snapshot when advertised.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    /// Concurrent instance ceiling.
    pub max_instances: i64,
    /// Hostname or SSH alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Hub-supervised SSH target and binary policy, absent for externally enrolled Nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh: Option<Value>,
    /// Latest SSH preflight/connection error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Instance index row.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRecord {
    /// `ins_…`.
    pub instance_id: String,
    /// Owning host.
    pub host_id: String,
    /// Optional workspace.
    pub workspace_id: Option<String>,
    /// Agent kind.
    pub kind: String,
    /// Driver kind.
    pub driver: String,
    /// Lifecycle.
    pub lifecycle: String,
    /// Activity knowledge or raw string.
    pub activity: String,
    /// Connectivity.
    pub connectivity: String,
    /// UI title.
    pub title: Option<String>,
    /// Live name (from spec).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Working directory recorded on the workspace / spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Provider delegation persisted from the create spec (`none` / `gateway`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<String>,
    /// Provider profile id persisted from the create spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile_id: Option<String>,
    /// Current model id from create / `instance.configure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Native effort tier name.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "effortName"
    )]
    pub effort_name: Option<String>,
    /// Native effort tier index.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "effortIndex"
    )]
    pub effort_index: Option<u32>,
    /// Journal id (`obj_…`).
    pub journal_id: String,
    /// Durable seq as decimal string.
    pub durable_seq: String,
    /// Create-time.
    pub created_at: String,
    /// Update-time.
    pub updated_at: String,
    /// Last native/driver error when lifecycle is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Command ledger row (three-state).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandRecord {
    /// `cmd_…`.
    pub command_id: String,
    /// Target instance if any.
    pub instance_id: Option<String>,
    /// Target host.
    pub host_id: String,
    /// Wire operation.
    pub operation: String,
    /// `queued` / `accepted` / `settled`.
    pub state: String,
    /// `clear` / `unknown`.
    pub resolution: String,
    /// True after Hub persisted a forward intent (never resend).
    pub forwarded: bool,
    /// Original payload.
    pub payload: Value,
    /// Optional caller idempotency key.
    pub idempotency_key: Option<String>,
    /// Create-time.
    pub created_at: String,
    /// Update-time.
    pub updated_at: String,
}

/// Mirrored journal event.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalRecord {
    /// Instance journal.
    pub instance_id: String,
    /// Monotonic seq from 1.
    pub seq: i64,
    /// `evt_…`.
    pub event_id: String,
    /// Opaque event JSON (Node is the authority).
    pub event: Value,
    /// Observed-at.
    pub observed_at: String,
}

/// Result of [`Store::append_journal`].
#[derive(Clone, Debug)]
pub struct JournalAppend {
    /// Mirrored row (existing row when `replayed`).
    pub record: JournalRecord,
    /// True when this seq was already durable; callers must not fan out again.
    pub replayed: bool,
    /// Inclusive instance watermark after this call (may exceed `record.seq` on replay).
    pub durable_seq: i64,
}

/// Hub-side journal resume cursor for one instance.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceWatermark {
    /// Instance journal owner.
    pub instance_id: String,
    /// Journal id (`obj_…`).
    pub journal_id: String,
    /// Inclusive durable seq as a decimal string.
    pub durable_seq: String,
}

/// Pending (or resolved) Interaction mirrored for Hub restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRecord {
    /// `int_…`.
    pub interaction_id: String,
    /// Owning instance.
    pub instance_id: String,
    /// Owning host.
    pub host_id: String,
    /// Interaction kind (`permission`, `question`, …).
    pub kind: String,
    /// `pending` / `answered` / `expired`.
    pub state: String,
    /// Blocks the instance while pending.
    pub blocking: bool,
    /// Source event JSON.
    pub payload: Value,
    /// Create-time.
    pub created_at: String,
    /// Update-time.
    pub updated_at: String,
}

/// Hub registry row for a gateway/direct provider profile. Secret bytes stay in the vault.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRecord {
    /// `pvp_…`.
    pub id: String,
    /// Operator label.
    pub name: String,
    /// `gateway` or `direct`.
    pub kind: String,
    /// Ingress base URL.
    pub base_url: String,
    /// Catalog model ids.
    pub models: Vec<String>,
    /// Prefill for New Session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Extra HTTP headers (never the auth token).
    pub headers: BTreeMap<String, String>,
    /// New Session `delegation=gateway` selects this profile.
    pub default_gateway: bool,
    /// Monotonic revision.
    pub revision: i64,
    /// Vault key (`provider-<id>`); omitted from GET JSON.
    #[serde(skip)]
    pub secret_name: Option<String>,
    /// Token is present in the vault.
    pub secret_present: bool,
    /// Last four UTF-8 characters of the token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_last4: Option<String>,
    /// SHA-256 prefix (16 hex chars).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_fingerprint: Option<String>,
    /// Last `/test` result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test_ok: Option<bool>,
    /// Last `/test` time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test_at: Option<String>,
    /// Last `/test` message (no secrets).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test_message: Option<String>,
    /// Create-time.
    pub created_at: String,
    /// Update-time.
    pub updated_at: String,
}

impl ProviderRecord {
    /// Public REST view. Never includes the auth token.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "baseUrl": self.base_url,
            "models": self.models,
            "defaultModel": self.default_model,
            "headers": self.headers,
            "defaultGateway": self.default_gateway,
            "revision": self.revision.to_string(),
            "secret": {
                "present": self.secret_present,
                "last4": self.secret_last4,
                "fingerprint": self.secret_fingerprint,
            },
            "health": self.last_test_ok.map(|ok| json!({
                "ok": ok,
                "checkedAt": self.last_test_at,
                "message": self.last_test_message,
            })),
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }

    /// Overlay spec threaded into `instance.create` (no token).
    pub fn overlay_spec(&self, model: Option<&str>) -> Value {
        let model = model
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or(self.default_model.as_deref())
            .or(self.models.first().map(String::as_str))
            .unwrap_or("");
        json!({
            "profileId": self.id,
            "kind": self.kind,
            "baseUrl": self.base_url,
            "model": model,
            "headers": self.headers,
        })
    }
}

impl InteractionRecord {
    /// REST list item.
    pub fn to_list_item(&self) -> Value {
        let mut item = json!({
            "id": self.interaction_id,
            "interactionId": self.interaction_id,
            "instanceId": self.instance_id,
            "hostId": self.host_id,
            "kind": self.kind,
            "state": self.state,
            "blocking": self.blocking,
            "event": self.payload,
        });
        if let Some(entity) = self
            .payload
            .pointer("/payload/interaction")
            .or_else(|| self.payload.pointer("/payload/entity"))
            && let Some(object) = item.as_object_mut()
        {
            if let Some(fields) = entity.as_object() {
                object.extend(fields.clone());
            }
            object.insert("state".into(), json!(self.state));
            object.insert("interaction".into(), entity.clone());
        }
        item
    }
}

impl Store {
    /// Open (or create) `hub.sqlite` on a dedicated writer thread.
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(data_dir).map_err(|err| StoreError::Id(err.to_string()))?;
        let path = data_dir.join("hub.sqlite");
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let thread = thread::Builder::new()
            .name("remuda-hub-sqlite".into())
            .spawn(move || {
                let mut conn = match open_conn(&path) {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(error = %err, "hub sqlite open failed");
                        return;
                    }
                };
                while let Ok(job) = rx.recv() {
                    match job {
                        Job::Run(work) => work(&mut conn),
                        Job::Stop(done) => {
                            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                            drop(conn);
                            let _ = done.send(());
                            return;
                        }
                    }
                }
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
            })
            .map_err(|err| StoreError::Id(err.to_string()))?;
        Ok(Self {
            tx,
            _join: Arc::new(StoreJoin {
                thread: Mutex::new(Some(thread)),
            }),
        })
    }

    /// Finish in-flight jobs, checkpoint WAL, and close the writer connection.
    pub async fn close(&self) {
        let (done, rx) = std::sync::mpsc::channel();
        if self.tx.send(Job::Stop(done)).is_err() {
            return;
        }
        let _ = tokio::task::spawn_blocking(move || rx.recv_timeout(BUSY_WAIT)).await;
    }

    pub(crate) async fn run<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Job::Run(Box::new(move |conn| {
                let _ = tx.send(f(conn));
            })))
            .map_err(|_| StoreError::Closed)?;
        rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Insert a device token hash.
    pub async fn insert_device(
        &self,
        name: String,
        token_hash: String,
        token_prefix: String,
    ) -> Result<Device, StoreError> {
        self.run(move |conn| {
            let id = new_id("dev").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO devices (id, name, token_hash, created_at, last_seen_at, token_prefix)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                params![id, name, token_hash, now, token_prefix],
            )?;
            Ok(Device { id, name })
        })
        .await
    }

    /// Indexed lookup, followed by one full-token verification. Legacy cookies
    /// may migrate using an explicit device id, never a scan of salted hashes.
    pub async fn find_device_by_token<F>(
        &self,
        token: String,
        legacy_device_id: Option<String>,
        verify: F,
    ) -> Result<Option<Device>, StoreError>
    where
        F: Fn(&str, &str) -> bool + Send + 'static,
    {
        self.run(move |conn| {
            let Some(prefix) = crate::auth::token_prefix(&token) else { return Ok(None); };
            let read_row = |row: &rusqlite::Row<'_>| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            };
            let mut candidate = conn.query_row(
                "SELECT id, name, token_hash FROM devices WHERE token_prefix = ?1",
                params![prefix], read_row,
            ).optional()?;
            if candidate.is_none() && let Some(id) = legacy_device_id {
                candidate = conn.query_row(
                    "SELECT id, name, token_hash FROM devices WHERE id = ?1 AND token_prefix IS NULL",
                    params![id], read_row,
                ).optional()?;
            }
            let Some((id, name, hash)) = candidate else { return Ok(None); };
            if !verify(&token, &hash) { return Ok(None); }
            conn.execute(
                "UPDATE devices SET last_seen_at = ?1, token_prefix = ?2 WHERE id = ?3",
                params![now_rfc3339(), prefix, id],
            )?;
            Ok(Some(Device { id, name }))
        })
        .await
    }

    /// Enroll or refresh a host using `verify` against stored token hashes.
    pub async fn authenticate_host<F>(
        &self,
        request: HostAuthRequest,
        verify: F,
        hash_new: impl Fn(&str) -> Result<String, StoreError> + Send + 'static,
    ) -> Result<HostAuthOutcome, StoreError>
    where
        F: Fn(&str, &str) -> bool + Send + 'static,
    {
        self.run(move |conn| {
            let prefix = crate::auth::token_prefix(&request.presented);
            let read_row = |row: &rusqlite::Row<'_>| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            };
            let mut candidate = conn.query_row(
                "SELECT id, token_hash FROM hosts WHERE token_prefix = ?1",
                params![prefix], read_row,
            ).optional()?;
            if candidate.is_none() && prefix.is_some() && let Some(id) = &request.hello_host_id {
                candidate = conn.query_row(
                    "SELECT id, token_hash FROM hosts WHERE id = ?1 AND token_prefix IS NULL",
                    params![id], read_row,
                ).optional()?;
            }
            if let Some((id, hash)) = candidate
                && verify(&request.presented, &hash) {
                    conn.execute("UPDATE hosts SET token_prefix = ?1 WHERE id = ?2", params![prefix, id])?;
                    let host = touch_host_online(conn, &id, &request.node_version)?;
                    return Ok(HostAuthOutcome::Authenticated {
                        host: Box::new(host),
                        node_token: None,
                    });
            }
            if !crate::config::secret_eq(&request.presented, &request.bootstrap) {
                return Ok(HostAuthOutcome::Rejected);
            }
            let host_id = match request.hello_host_id {
                Some(id) => id,
                None => new_id("hst").map_err(|e| StoreError::Id(e.to_string()))?,
            };
            if load_host(conn, &host_id)?.is_some() {
                // Bootstrap authorizes enrollment only. Reconnecting to an
                // existing identity requires its own host token above.
                return Ok(HostAuthOutcome::Rejected);
            }
            let node_token = crate::config::random_token();
            let token_hash = hash_new(&node_token)?;
            let now = now_rfc3339();
            let label = request.label.unwrap_or_else(|| host_id.clone());
            conn.execute(
                "INSERT INTO hosts
                    (id, label, token_hash, state, last_seen_at, node_version, cli_json, capabilities_json,
                     created_at, transport, labels_json, herdr_json, resources_json, max_instances, hostname, token_prefix)
                 VALUES (?1, ?2, ?3, 'online', ?4, ?5, '[]', '{}', ?4, 'outbound-wss', '[]', NULL, NULL, 8, NULL, ?6)",
                params![host_id, label, token_hash, now, request.node_version, crate::auth::token_prefix(&node_token)],
            )?;
            let host = load_host(conn, &host_id)?
                .ok_or_else(|| StoreError::Id("host insert missing".into()))?;
            Ok(HostAuthOutcome::Authenticated {
                host: Box::new(host),
                node_token: Some(node_token),
            })
        })
        .await
    }

    /// Mark every host offline. Hub restart has no live Node links until hello.
    pub async fn mark_all_hosts_offline(&self) -> Result<(), StoreError> {
        self.run(|conn| {
            conn.execute(
                // SSH inventory is collected at connect, so last_seen_at may
                // predate a still-live carrier. Start its lost grace at restart.
                "UPDATE hosts SET state = 'offline', offline_since = COALESCE(offline_since,
                    CASE WHEN EXISTS (SELECT 1 FROM ssh_hosts WHERE host_id = hosts.id)
                    THEN ?1 ELSE COALESCE(last_seen_at, created_at) END) WHERE state = 'online'",
                params![now_rfc3339()],
            )?;
            conn.execute(
                "UPDATE instances SET connectivity = 'disconnected', updated_at = ?1
                 WHERE connectivity != 'disconnected'",
                params![now_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    /// Overlay Hub<->Node liveness onto a stored host row.
    #[must_use]
    pub fn with_live_link(mut host: HostRecord, connected: bool) -> HostRecord {
        // A supervised SSH link is ready only after hello has been acknowledged.
        // Retirement also fences placement before the carrier is torn down.
        host.online =
            connected && host.state != "retired" && (host.ssh.is_none() || host.state == "online");
        if host.online {
            host.state = "online".into();
        } else if host.state == "online" {
            host.state = "offline".into();
        }
        host
    }

    /// Mark a host offline when its WS drops.
    pub async fn mark_host_offline(&self, host_id: String) -> Result<(), StoreError> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE hosts SET state = 'offline', offline_since = ?2 WHERE id = ?1 AND state = 'online'",
                params![&host_id, now_rfc3339()],
            )?;
            conn.execute(
                "UPDATE instances SET connectivity = 'disconnected', updated_at = ?1
                 WHERE host_id = ?2 AND connectivity != 'disconnected'",
                params![now_rfc3339(), host_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Hub-owned projection only: never forge a Node journal cursor or native completion.
    pub async fn expire_lost_hosts(&self, grace_ms: u64) -> Result<usize, StoreError> {
        self.run(move |conn| {
            let changed = conn.execute(
                "UPDATE instances SET lifecycle = 'exited', activity = 'idle',
                    connectivity = 'disconnected', last_error = 'host-lost', updated_at = ?1
                 WHERE lifecycle NOT IN ('exited', 'closed') AND host_id IN (
                    SELECT id FROM hosts WHERE state != 'online' AND
                    (NOT EXISTS (SELECT 1 FROM ssh_hosts WHERE host_id = hosts.id) OR state = 'daemon-unreachable') AND
                    (julianday(?1) - julianday(COALESCE(offline_since, last_seen_at, created_at))) * 86400000 >= ?2
                 )",
                params![now_rfc3339(), grace_ms.min(i64::MAX as u64) as i64],
            )?;
            Ok(changed)
        }).await
    }

    /// Heartbeat / hello: lastSeen + optional inventory (D-013).
    pub async fn apply_inventory(
        &self,
        host_id: String,
        update: crate::inventory::HostInventoryUpdate,
        capabilities: Option<Value>,
    ) -> Result<HostRecord, StoreError> {
        self.run(move |conn| {
            let now = now_rfc3339();
            conn.execute(
                "UPDATE hosts SET last_seen_at = ?1, state = CASE WHEN EXISTS (SELECT 1 FROM ssh_hosts WHERE host_id = hosts.id) THEN state ELSE 'online' END, offline_since = NULL WHERE id = ?2 AND state != 'retired'",
                params![&now, &host_id],
            )?;
            conn.execute(
                "UPDATE instances SET connectivity = 'connected', updated_at = ?1
                 WHERE host_id = ?2 AND connectivity != 'connected'",
                params![&now, &host_id],
            )?;
            if let Some(label) = update.display_label {
                conn.execute(
                    "UPDATE hosts SET label = ?1 WHERE id = ?2",
                    params![label, host_id],
                )?;
            }
            if let Some(version) = update.node_version {
                conn.execute(
                    "UPDATE hosts SET node_version = ?1 WHERE id = ?2",
                    params![version, host_id],
                )?;
            }
            if let Some(cli) = update.cli {
                conn.execute(
                    "UPDATE hosts SET cli_json = ?1 WHERE id = ?2",
                    params![cli.to_string(), host_id],
                )?;
            }
            if let Some(caps) = capabilities {
                conn.execute(
                    "UPDATE hosts SET capabilities_json = ?1 WHERE id = ?2",
                    params![caps.to_string(), host_id],
                )?;
            }
            if let Some(labels) = update.labels {
                conn.execute(
                    "UPDATE hosts SET labels_json = ?1 WHERE id = ?2",
                    params![labels.to_string(), host_id],
                )?;
            }
            if let Some(herdr) = update.herdr {
                conn.execute(
                    "UPDATE hosts SET herdr_json = ?1 WHERE id = ?2",
                    params![herdr.to_string(), host_id],
                )?;
            }
            if let Some(resources) = update.resources {
                conn.execute(
                    "UPDATE hosts SET resources_json = ?1 WHERE id = ?2",
                    params![resources.to_string(), host_id],
                )?;
            }
            if let Some(max_instances) = update.max_instances {
                conn.execute(
                    "UPDATE hosts SET max_instances = ?1 WHERE id = ?2",
                    params![max_instances, host_id],
                )?;
            }
            if let Some(hostname) = update.hostname {
                conn.execute(
                    "UPDATE hosts SET hostname = ?1 WHERE id = ?2",
                    params![hostname, host_id],
                )?;
            }
            if let Some(transport) = update.transport {
                conn.execute(
                    "UPDATE hosts SET transport = ?1 WHERE id = ?2",
                    params![transport.as_str(), host_id],
                )?;
            }
            load_host(conn, &host_id)?.ok_or_else(|| StoreError::Id("unknown host".into()))
        })
        .await
    }

    /// All hosts.
    pub async fn list_hosts(&self) -> Result<Vec<HostRecord>, StoreError> {
        self.run(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM hosts ORDER BY created_at")?;
            let ids: Vec<String> = stmt
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            ids.into_iter()
                .map(|id| {
                    load_host(conn, &id)?
                        .ok_or_else(|| StoreError::Id("host missing during list".into()))
                })
                .collect()
        })
        .await
    }

    /// One host.
    pub async fn get_host(&self, host_id: String) -> Result<Option<HostRecord>, StoreError> {
        self.run(move |conn| load_host(conn, &host_id)).await
    }

    /// Insert an instance index row.
    pub async fn insert_instance(
        &self,
        host_id: String,
        workspace_id: Option<String>,
        kind: String,
        driver: String,
        title: Option<String>,
        spec: Value,
    ) -> Result<InstanceRecord, StoreError> {
        self.run(move |conn| {
            let host =
                load_host(conn, &host_id)?.ok_or_else(|| StoreError::Id("unknown host".into()))?;
            let running: i64 = conn.query_row(
                "SELECT COUNT(*) FROM instances
                 WHERE host_id = ?1 AND lifecycle NOT IN ('exited', 'failed')",
                params![host_id],
                |row| row.get(0),
            )?;
            if running >= host.max_instances {
                return Err(StoreError::Id(format!(
                    "{}: at maxInstances {}",
                    host.host_id, host.max_instances
                )));
            }
            if host.ssh.is_some() && !host.online {
                return Err(StoreError::Id(
                    "SSH host is not online; retry after reconnect".into(),
                ));
            }
            let instance_id = new_id("ins").map_err(|e| StoreError::Id(e.to_string()))?;
            let journal_id = new_id("obj").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            let connectivity = if host.online {
                "connected"
            } else {
                "disconnected"
            };
            conn.execute(
                "INSERT INTO instances
                    (id, host_id, workspace_id, kind, driver, lifecycle, activity, connectivity,
                     title, journal_id, durable_seq, spec_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'requested', 'unknown', ?6,
                         ?7, ?8, 0, ?9, ?10, ?10)",
                params![
                    instance_id,
                    host_id,
                    workspace_id,
                    kind,
                    driver,
                    connectivity,
                    title,
                    journal_id,
                    spec.to_string(),
                    now
                ],
            )?;
            load_instance(conn, &instance_id)?
                .ok_or_else(|| StoreError::Id("instance insert missing".into()))
        })
        .await
    }

    /// Ensure an instance row exists so Node can append journal before HTTP create.
    pub async fn ensure_instance(
        &self,
        host_id: String,
        instance_id: String,
    ) -> Result<InstanceRecord, StoreError> {
        self.run(move |conn| {
            if let Some(existing) = load_instance(conn, &instance_id)? {
                if existing.host_id != host_id {
                    return Err(StoreError::Id("instance belongs to another host".into()));
                }
                return Ok(existing);
            }
            let journal_id = new_id("obj").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO instances
                    (id, host_id, workspace_id, kind, driver, lifecycle, activity, connectivity,
                     title, journal_id, durable_seq, spec_json, created_at, updated_at)
                 VALUES (?1, ?2, NULL, 'claude', 'claude-print', 'requested', 'unknown', 'connected',
                         NULL, ?3, 0, '{}', ?4, ?4)",
                params![instance_id, host_id, journal_id, now],
            )?;
            load_instance(conn, &instance_id)?
                .ok_or_else(|| StoreError::Id("ensure instance missing".into()))
        })
        .await
    }

    /// List instances, optionally filtered by host.
    pub async fn list_instances(
        &self,
        host_id: Option<String>,
    ) -> Result<Vec<InstanceRecord>, StoreError> {
        self.run(move |conn| {
            let sql = if host_id.is_some() {
                "SELECT id FROM instances WHERE host_id = ?1 ORDER BY updated_at DESC"
            } else {
                "SELECT id FROM instances ORDER BY updated_at DESC"
            };
            let mut stmt = conn.prepare(sql)?;
            let ids: Vec<String> = if let Some(host_id) = host_id {
                stmt.query_map(params![host_id], |row| row.get(0))?
                    .collect::<Result<_, _>>()?
            } else {
                stmt.query_map([], |row| row.get(0))?
                    .collect::<Result<_, _>>()?
            };
            drop(stmt);
            ids.into_iter()
                .map(|id| {
                    load_instance(conn, &id)?
                        .ok_or_else(|| StoreError::Id("instance missing during list".into()))
                })
                .collect()
        })
        .await
    }

    /// One instance.
    pub async fn get_instance(
        &self,
        instance_id: String,
    ) -> Result<Option<InstanceRecord>, StoreError> {
        self.run(move |conn| load_instance(conn, &instance_id))
            .await
    }

    /// Merge model / effort into the instance spec so reload and list rows see them.
    pub async fn patch_instance_configure(
        &self,
        instance_id: String,
        payload: Value,
    ) -> Result<InstanceRecord, StoreError> {
        self.run(move |conn| {
            let spec_raw: String = conn.query_row(
                "SELECT spec_json FROM instances WHERE id = ?1",
                params![instance_id],
                |row| row.get(0),
            )?;
            let spec: Value = serde_json::from_str(&spec_raw).unwrap_or(json!({}));
            let Some(mut object) = spec.as_object().cloned() else {
                return load_instance(conn, &instance_id)?
                    .ok_or_else(|| StoreError::Id("unknown instance".into()));
            };
            if let Some(model) = payload
                .get("model")
                .or_else(|| payload.get("modelId"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                object.insert("model".into(), json!(model));
            }
            if let Some(effort) = payload.get("effort") {
                object.insert("effort".into(), effort.clone());
            } else if payload.get("effortName").is_some() || payload.get("effortIndex").is_some() {
                object.insert(
                    "effort".into(),
                    json!({
                        "name": payload.get("effortName"),
                        "index": payload.get("effortIndex"),
                    }),
                );
            }
            if let Some(mode) = payload.get("permissionMode").and_then(Value::as_str) {
                object.insert("permissionMode".into(), json!(mode));
            }
            let now = now_rfc3339();
            conn.execute(
                "UPDATE instances SET spec_json = ?1, updated_at = ?2 WHERE id = ?3",
                params![Value::Object(object).to_string(), now, instance_id],
            )?;
            load_instance(conn, &instance_id)?
                .ok_or_else(|| StoreError::Id("unknown instance".into()))
        })
        .await
    }

    /// Queue a command. Same `command_id` or idempotency key returns the original row.
    pub async fn queue_command(
        &self,
        command_id: Option<String>,
        instance_id: Option<String>,
        host_id: String,
        operation: String,
        payload: Value,
        idempotency_key: Option<String>,
    ) -> Result<(CommandRecord, bool), StoreError> {
        self.run(move |conn| {
            if let Some(key) = idempotency_key.as_ref()
                && let Some(existing) = load_command_by_key(conn, key)?
            {
                if existing.payload != payload || existing.operation != operation {
                    return Err(StoreError::Id(
                        "idempotency key reused with a different payload".into(),
                    ));
                }
                return Ok((existing, false));
            }
            let command_id = match command_id {
                Some(id) => {
                    if let Some(existing) = load_command(conn, &id)? {
                        if existing.payload != payload || existing.operation != operation {
                            return Err(StoreError::Id(
                                "commandId reused with a different payload".into(),
                            ));
                        }
                        return Ok((existing, false));
                    }
                    id
                }
                None => new_id("cmd").map_err(|e| StoreError::Id(e.to_string()))?,
            };
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO commands
                    (id, instance_id, host_id, operation, state, resolution, forwarded,
                     payload_json, idempotency_key, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'queued', 'clear', 0, ?5, ?6, ?7, ?7)",
                params![
                    command_id,
                    instance_id,
                    host_id,
                    operation,
                    payload.to_string(),
                    idempotency_key,
                    now
                ],
            )?;
            let row = load_command(conn, &command_id)?
                .ok_or_else(|| StoreError::Id("command insert missing".into()))?;
            Ok((row, true))
        })
        .await
    }

    /// Persist forward intent. Returns false if already forwarded (do not resend).
    pub async fn mark_forward_intent(&self, command_id: String) -> Result<bool, StoreError> {
        self.run(move |conn| {
            let Some(row) = load_command(conn, &command_id)? else {
                return Err(StoreError::Id("unknown command".into()));
            };
            if row.forwarded {
                return Ok(false);
            }
            let now = now_rfc3339();
            conn.execute(
                "UPDATE commands SET forwarded = 1, resolution = 'unknown', updated_at = ?1 WHERE id = ?2",
                params![now, command_id],
            )?;
            Ok(true)
        })
        .await
    }

    /// Node RPC success → `accepted`.
    pub async fn mark_accepted(&self, command_id: String) -> Result<CommandRecord, StoreError> {
        self.run(move |conn| {
            let now = now_rfc3339();
            conn.execute(
                "UPDATE commands SET state = 'accepted', resolution = 'clear', updated_at = ?1
                 WHERE id = ?2 AND state = 'queued'",
                params![now, command_id],
            )?;
            load_command(conn, &command_id)?.ok_or_else(|| StoreError::Id("unknown command".into()))
        })
        .await
    }

    /// Expire the create settlement watch without changing its three-state progress.
    pub async fn mark_settlement_timed_out(
        &self,
        command_id: String,
    ) -> Result<Option<CommandRecord>, StoreError> {
        self.run(move |conn| {
            let now = now_rfc3339();
            let changed = conn.execute(
                "UPDATE commands SET resolution = 'unknown', updated_at = ?1
                 WHERE id = ?2 AND state = 'accepted' AND resolution = 'clear'",
                params![now, command_id],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            load_command(conn, &command_id)
        })
        .await
    }

    /// Node-reported completion → `settled`.
    pub async fn mark_settled(
        &self,
        command_id: String,
        host_id: String,
    ) -> Result<CommandRecord, StoreError> {
        self.run(move |conn| {
            let Some(row) = load_command(conn, &command_id)? else {
                return Err(StoreError::Id("unknown command".into()));
            };
            if row.host_id != host_id {
                return Err(StoreError::Id("command belongs to another host".into()));
            }
            let now = now_rfc3339();
            conn.execute(
                "UPDATE commands SET state = 'settled', resolution = 'clear', updated_at = ?1 WHERE id = ?2",
                params![now, command_id],
            )?;
            load_command(conn, &command_id)?
                .ok_or_else(|| StoreError::Id("unknown command".into()))
        })
        .await
    }

    /// Load a command.
    pub async fn get_command(
        &self,
        command_id: String,
    ) -> Result<Option<CommandRecord>, StoreError> {
        self.run(move |conn| load_command(conn, &command_id)).await
    }

    /// Append a mirrored event. `seq` None assigns durableSeq+1.
    pub async fn append_journal(
        &self,
        host_id: String,
        instance_id: String,
        seq: Option<i64>,
        mut event: Value,
    ) -> Result<JournalAppend, StoreError> {
        self.run(move |conn| {
            let inst = load_instance(conn, &instance_id)?
                .ok_or_else(|| StoreError::Id("unknown instance".into()))?;
            if inst.host_id != host_id {
                return Err(StoreError::Id("instance belongs to another host".into()));
            }
            let next = inst.durable_seq.parse::<i64>().unwrap_or(0) + 1;
            let seq = seq.unwrap_or(next);
            let durable = inst.durable_seq.parse::<i64>().unwrap_or(0);
            if let Some(existing) = load_journal_row(conn, &instance_id, seq)? {
                apply_interaction_event(conn, &host_id, &instance_id, &existing.event)?;
                return Ok(JournalAppend {
                    record: existing,
                    replayed: true,
                    durable_seq: durable,
                });
            }
            if seq != next {
                return Err(StoreError::Id(format!(
                    "journal gap: expected {next}, got {seq}"
                )));
            }
            let event_id = event
                .get("eventId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| new_id("evt").unwrap_or_else(|_| "evt_missing".into()));
            if let Some(obj) = event.as_object_mut() {
                obj.entry("eventId".to_string())
                    .or_insert_with(|| json!(event_id.clone()));
                obj.entry("seq".to_string())
                    .or_insert_with(|| json!(seq.to_string()));
                obj.entry("instanceId".to_string())
                    .or_insert_with(|| json!(instance_id.clone()));
            }
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![instance_id, seq, event_id, event.to_string(), now],
            )?;
            apply_instance_projection(conn, &instance_id, &event, seq, &now)?;
            apply_command_projection(conn, &host_id, &instance_id, &event, &now)?;
            apply_interaction_event(conn, &host_id, &instance_id, &event)?;
            apply_instance_lifecycle(conn, &instance_id, &event)?;
            Ok(JournalAppend {
                record: JournalRecord {
                    instance_id,
                    seq,
                    event_id,
                    event,
                    observed_at: now,
                },
                replayed: false,
                durable_seq: seq,
            })
        })
        .await
    }

    /// Inclusive durable-seq watermarks for every instance on `host_id`.
    pub async fn list_instance_watermarks(
        &self,
        host_id: String,
    ) -> Result<Vec<InstanceWatermark>, StoreError> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, journal_id, durable_seq FROM instances WHERE host_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![host_id], |row| {
                let durable: i64 = row.get(2)?;
                Ok(InstanceWatermark {
                    instance_id: row.get(0)?,
                    journal_id: row.get(1)?,
                    durable_seq: durable.to_string(),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
        .await
    }

    /// Pending interactions, optionally filtered.
    pub async fn list_interactions(
        &self,
        host_id: Option<String>,
        instance_id: Option<String>,
        kind: Option<String>,
        pending_only: bool,
    ) -> Result<Vec<InteractionRecord>, StoreError> {
        self.run(move |conn| {
            let mut sql = String::from(
                "SELECT id, instance_id, host_id, kind, state, blocking, payload_json, created_at, updated_at
                 FROM interactions WHERE 1=1",
            );
            let mut args: Vec<String> = Vec::new();
            if let Some(host_id) = &host_id {
                sql.push_str(" AND host_id = ?");
                args.push(host_id.clone());
            }
            if let Some(instance_id) = &instance_id {
                sql.push_str(" AND instance_id = ?");
                args.push(instance_id.clone());
            }
            if let Some(kind) = &kind {
                sql.push_str(" AND kind = ?");
                args.push(kind.clone());
            }
            if pending_only {
                sql.push_str(" AND state = 'pending'");
            }
            sql.push_str(" ORDER BY created_at");
            let mut stmt = conn.prepare(&sql)?;
            let params_refs: Vec<&dyn rusqlite::types::ToSql> = args
                .iter()
                .map(|s| s as &dyn rusqlite::types::ToSql)
                .collect();
            let rows = stmt.query_map(params_refs.as_slice(), interaction_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
        .await
    }

    /// One interaction by id.
    pub async fn get_interaction(
        &self,
        interaction_id: String,
    ) -> Result<Option<InteractionRecord>, StoreError> {
        self.run(move |conn| load_interaction(conn, &interaction_id))
            .await
    }

    /// Mirror a successful Node answer ACK; never decide a winner in the Hub.
    pub async fn record_interaction_answer(
        &self,
        interaction_id: String,
    ) -> Result<(), StoreError> {
        self.run(move |conn| {
            conn.execute("UPDATE interactions SET state = 'answer-committed', updated_at = ?1 WHERE id = ?2 AND state = 'pending'", params![now_rfc3339(), interaction_id])?;
            Ok(())
        }).await
    }

    /// Journal page from `after_seq` exclusive.
    pub async fn read_journal(
        &self,
        instance_id: String,
        after_seq: i64,
    ) -> Result<(Vec<JournalRecord>, i64), StoreError> {
        self.run(move |conn| {
            let durable = load_instance(conn, &instance_id)?
                .map(|i| i.durable_seq.parse::<i64>().unwrap_or(0))
                .unwrap_or(0);
            let mut stmt = conn.prepare(
                "SELECT seq, event_id, payload_json, observed_at FROM journal
                 WHERE instance_id = ?1 AND seq > ?2 ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map(params![instance_id, after_seq], |row| {
                let payload: String = row.get(2)?;
                Ok(JournalRecord {
                    instance_id: instance_id.clone(),
                    seq: row.get(0)?,
                    event_id: row.get(1)?,
                    event: serde_json::from_str(&payload).unwrap_or(Value::Null),
                    observed_at: row.get(3)?,
                })
            })?;
            let events = rows.collect::<Result<Vec<_>, _>>()?;
            Ok((events, durable))
        })
        .await
    }

    /// Operator PATCH of labels / maxInstances / display name (does not mark online).
    pub async fn patch_host(
        &self,
        host_id: String,
        name: Option<String>,
        labels: Option<Value>,
        max_instances: Option<i64>,
    ) -> Result<HostRecord, StoreError> {
        self.run(move |conn| {
            if load_host(conn, &host_id)?.is_none() {
                return Err(StoreError::Id("unknown host".into()));
            }
            if let Some(name) = name {
                conn.execute(
                    "UPDATE hosts SET label = ?1 WHERE id = ?2",
                    params![name, host_id],
                )?;
            }
            if let Some(labels) = labels {
                conn.execute(
                    "UPDATE hosts SET labels_json = ?1 WHERE id = ?2",
                    params![labels.to_string(), host_id],
                )?;
            }
            if let Some(max_instances) = max_instances {
                conn.execute(
                    "UPDATE hosts SET max_instances = ?1 WHERE id = ?2",
                    params![max_instances, host_id],
                )?;
            }
            load_host(conn, &host_id)?.ok_or_else(|| StoreError::Id("unknown host".into()))
        })
        .await
    }

    /// Instances that still occupy a concurrency slot.
    pub async fn running_count(&self, host_id: String) -> Result<i64, StoreError> {
        self.run(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM instances
                 WHERE host_id = ?1 AND lifecycle NOT IN ('exited', 'failed')",
                params![host_id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
        })
        .await
    }

    /// Persist a fleet and its members.
    pub async fn insert_fleet(
        &self,
        spec: Value,
        members: Vec<(String, String)>,
    ) -> Result<String, StoreError> {
        self.run(move |conn| {
            let fleet_id = new_id("obj").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO fleets (id, spec_json, created_at) VALUES (?1, ?2, ?3)",
                params![fleet_id, spec.to_string(), now],
            )?;
            for (instance_id, host_id) in members {
                conn.execute(
                    "INSERT INTO fleet_members (fleet_id, instance_id, host_id) VALUES (?1, ?2, ?3)",
                    params![fleet_id, instance_id, host_id],
                )?;
            }
            Ok(fleet_id)
        })
        .await
    }

    /// Fleet spec + member instance ids.
    pub async fn get_fleet(
        &self,
        fleet_id: String,
    ) -> Result<Option<(Value, Vec<(String, String)>)>, StoreError> {
        self.run(move |conn| {
            let spec: Option<String> = conn
                .query_row(
                    "SELECT spec_json FROM fleets WHERE id = ?1",
                    params![fleet_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(spec) = spec else {
                return Ok(None);
            };
            let mut stmt = conn.prepare(
                "SELECT instance_id, host_id FROM fleet_members WHERE fleet_id = ?1 ORDER BY instance_id",
            )?;
            let members = stmt
                .query_map(params![fleet_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some((
                serde_json::from_str(&spec).unwrap_or(Value::Null),
                members,
            )))
        })
        .await
    }

    /// All paired devices (no token hashes).
    pub async fn list_devices(&self) -> Result<Vec<Device>, StoreError> {
        self.run(|conn| {
            let mut stmt = conn.prepare("SELECT id, name FROM devices ORDER BY created_at")?;
            let rows = stmt.query_map([], |row| {
                Ok(Device {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
        .await
    }

    /// Remove a device row.
    pub async fn delete_device(&self, device_id: String) -> Result<bool, StoreError> {
        self.run(move |conn| {
            let n = conn.execute("DELETE FROM devices WHERE id = ?1", params![device_id])?;
            Ok(n > 0)
        })
        .await
    }

    /// Store a hashed one-time pairing code.
    pub async fn insert_pair_code(
        &self,
        code_hash: String,
        code_prefix: String,
        created_by: String,
        expires_at: String,
    ) -> Result<bool, StoreError> {
        self.run(move |conn| {
            conn.execute("DELETE FROM pair_codes WHERE used != 0 OR expires_at <= ?1 OR failed_attempts >= ?2",
                params![now_rfc3339(), crate::auth::MAX_PAIR_FAILURES])?;
            let inserted = conn.execute(
                "INSERT INTO pair_codes (code_hash, created_by, expires_at, used, code_prefix)
                 VALUES (?1, ?2, ?3, 0, ?4) ON CONFLICT DO NOTHING",
                params![code_hash, created_by, expires_at, code_prefix],
            )?;
            Ok(inserted != 0)
        })
        .await
    }

    /// Consume a pairing code if it is unused and unexpired.
    pub async fn consume_pair_code<F>(
        &self,
        presented: String,
        now: String,
        verify: F,
    ) -> Result<bool, StoreError>
    where
        F: Fn(&str, &str) -> bool + Send + 'static,
    {
        self.run(move |conn| {
            let Some(prefix) = crate::auth::pair_prefix(&presented) else { return Ok(false); };
            let hash = conn.query_row(
                "SELECT code_hash FROM pair_codes WHERE code_prefix = ?1 AND used = 0 AND expires_at > ?2 AND failed_attempts < ?3",
                params![prefix, now, crate::auth::MAX_PAIR_FAILURES], |row| row.get::<_, String>(0),
            ).optional()?;
            let Some(hash) = hash else { return Ok(false); };
            let accepted = verify(&presented, &hash);
            if accepted {
                conn.execute("UPDATE pair_codes SET used = 1 WHERE code_hash = ?1", params![hash])?;
            } else {
                conn.execute("UPDATE pair_codes SET failed_attempts = failed_attempts + 1 WHERE code_hash = ?1", params![hash])?;
            }
            Ok(accepted)
        })
        .await
    }

    /// List provider profiles (metadata only).
    pub async fn list_providers(&self) -> Result<Vec<ProviderRecord>, StoreError> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, kind, base_url, models_json, default_model, headers_json,
                        is_default, revision, secret_name, secret_last4, secret_fingerprint,
                        last_test_ok, last_test_at, last_test_message, created_at, updated_at
                 FROM provider_profiles
                 ORDER BY is_default DESC, name COLLATE NOCASE ASC, id ASC",
            )?;
            let rows = stmt.query_map([], load_provider_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
        .await
    }

    /// Fetch one profile.
    pub async fn get_provider(&self, id: String) -> Result<Option<ProviderRecord>, StoreError> {
        self.run(move |conn| load_provider(conn, &id)).await
    }

    /// The profile marked default gateway, if any.
    pub async fn default_gateway(&self) -> Result<Option<ProviderRecord>, StoreError> {
        self.run(move |conn| {
            let id: Option<String> = conn
                .query_row(
                    "SELECT id FROM provider_profiles WHERE is_default = 1 AND kind = 'gateway' LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            match id {
                Some(id) => load_provider(conn, &id),
                None => Ok(None),
            }
        })
        .await
    }

    /// Insert a provider metadata row. Secret bytes belong in the vault.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_provider(
        &self,
        id: String,
        name: String,
        kind: String,
        base_url: String,
        models: Vec<String>,
        default_model: Option<String>,
        headers: BTreeMap<String, String>,
        default_gateway: bool,
        secret_name: Option<String>,
        secret_last4: Option<String>,
        secret_fingerprint: Option<String>,
    ) -> Result<ProviderRecord, StoreError> {
        self.run(move |conn| {
            let now = now_rfc3339();
            if default_gateway {
                conn.execute(
                    "UPDATE provider_profiles SET is_default = 0 WHERE is_default = 1",
                    [],
                )?;
            }
            conn.execute(
                "INSERT INTO provider_profiles
                 (id, name, kind, base_url, models_json, default_model, headers_json,
                  is_default, revision, secret_name, secret_last4, secret_fingerprint,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11, ?12, ?12)",
                params![
                    id,
                    name,
                    kind,
                    base_url,
                    serde_json::to_string(&models)?,
                    default_model,
                    serde_json::to_string(&headers)?,
                    i64::from(default_gateway),
                    secret_name,
                    secret_last4,
                    secret_fingerprint,
                    now,
                ],
            )?;
            load_provider(conn, &id)?
                .ok_or_else(|| StoreError::Id("provider insert missing".into()))
        })
        .await
    }

    /// Patch provider metadata. `clear_default` is unused; `default_gateway` is the source of truth.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_provider(
        &self,
        id: String,
        name: Option<String>,
        kind: Option<String>,
        base_url: Option<String>,
        models: Option<Vec<String>>,
        default_model: Option<Option<String>>,
        headers: Option<BTreeMap<String, String>>,
        default_gateway: Option<bool>,
        secret_name: Option<Option<String>>,
        secret_last4: Option<Option<String>>,
        secret_fingerprint: Option<Option<String>>,
    ) -> Result<ProviderRecord, StoreError> {
        self.run(move |conn| {
            if load_provider(conn, &id)?.is_none() {
                return Err(StoreError::Id("unknown provider".into()));
            }
            if default_gateway == Some(true) {
                conn.execute(
                    "UPDATE provider_profiles SET is_default = 0 WHERE is_default = 1 AND id != ?1",
                    params![id],
                )?;
            }
            if let Some(name) = name {
                conn.execute(
                    "UPDATE provider_profiles SET name = ?1 WHERE id = ?2",
                    params![name, id],
                )?;
            }
            if let Some(kind) = kind {
                conn.execute(
                    "UPDATE provider_profiles SET kind = ?1 WHERE id = ?2",
                    params![kind, id],
                )?;
            }
            if let Some(base_url) = base_url {
                conn.execute(
                    "UPDATE provider_profiles SET base_url = ?1 WHERE id = ?2",
                    params![base_url, id],
                )?;
            }
            if let Some(models) = models {
                conn.execute(
                    "UPDATE provider_profiles SET models_json = ?1 WHERE id = ?2",
                    params![serde_json::to_string(&models)?, id],
                )?;
            }
            if let Some(default_model) = default_model {
                conn.execute(
                    "UPDATE provider_profiles SET default_model = ?1 WHERE id = ?2",
                    params![default_model, id],
                )?;
            }
            if let Some(headers) = headers {
                conn.execute(
                    "UPDATE provider_profiles SET headers_json = ?1 WHERE id = ?2",
                    params![serde_json::to_string(&headers)?, id],
                )?;
            }
            if let Some(default_gateway) = default_gateway {
                conn.execute(
                    "UPDATE provider_profiles SET is_default = ?1 WHERE id = ?2",
                    params![i64::from(default_gateway), id],
                )?;
            }
            if let Some(secret_name) = secret_name {
                conn.execute(
                    "UPDATE provider_profiles SET secret_name = ?1 WHERE id = ?2",
                    params![secret_name, id],
                )?;
            }
            if let Some(secret_last4) = secret_last4 {
                conn.execute(
                    "UPDATE provider_profiles SET secret_last4 = ?1 WHERE id = ?2",
                    params![secret_last4, id],
                )?;
            }
            if let Some(secret_fingerprint) = secret_fingerprint {
                conn.execute(
                    "UPDATE provider_profiles SET secret_fingerprint = ?1 WHERE id = ?2",
                    params![secret_fingerprint, id],
                )?;
            }
            let now = now_rfc3339();
            conn.execute(
                "UPDATE provider_profiles SET revision = revision + 1, updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            load_provider(conn, &id)?.ok_or_else(|| StoreError::Id("unknown provider".into()))
        })
        .await
    }

    /// Record a `/test` probe on the profile (does not bump revision).
    pub async fn record_provider_test(
        &self,
        id: String,
        ok: bool,
        message: String,
    ) -> Result<ProviderRecord, StoreError> {
        self.run(move |conn| {
            if load_provider(conn, &id)?.is_none() {
                return Err(StoreError::Id("unknown provider".into()));
            }
            let now = now_rfc3339();
            conn.execute(
                "UPDATE provider_profiles
                 SET last_test_ok = ?1, last_test_at = ?2, last_test_message = ?3, updated_at = ?2
                 WHERE id = ?4",
                params![i64::from(ok), now, message, id],
            )?;
            load_provider(conn, &id)?.ok_or_else(|| StoreError::Id("unknown provider".into()))
        })
        .await
    }

    /// Delete a profile row. Caller deletes the vault entry.
    pub async fn delete_provider(&self, id: String) -> Result<Option<ProviderRecord>, StoreError> {
        self.run(move |conn| {
            let existing = load_provider(conn, &id)?;
            if existing.is_some() {
                conn.execute("DELETE FROM provider_profiles WHERE id = ?1", params![id])?;
            }
            Ok(existing)
        })
        .await
    }
}

fn load_provider(conn: &Connection, id: &str) -> Result<Option<ProviderRecord>, StoreError> {
    conn.query_row(
        "SELECT id, name, kind, base_url, models_json, default_model, headers_json,
                is_default, revision, secret_name, secret_last4, secret_fingerprint,
                last_test_ok, last_test_at, last_test_message, created_at, updated_at
         FROM provider_profiles WHERE id = ?1",
        params![id],
        load_provider_row,
    )
    .optional()
    .map_err(StoreError::from)
}

fn load_provider_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderRecord> {
    let models_json: String = row.get(4)?;
    let headers_json: String = row.get(6)?;
    let models: Vec<String> = serde_json::from_str(&models_json).unwrap_or_default();
    let headers: BTreeMap<String, String> = serde_json::from_str(&headers_json).unwrap_or_default();
    let secret_name: Option<String> = row.get(9)?;
    let last_test_ok: Option<i64> = row.get(12)?;
    Ok(ProviderRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        base_url: row.get(3)?,
        models,
        default_model: row.get(5)?,
        headers,
        default_gateway: row.get::<_, i64>(7)? != 0,
        revision: row.get(8)?,
        secret_present: secret_name.as_ref().is_some_and(|s| !s.is_empty()),
        secret_name,
        secret_last4: row.get(10)?,
        secret_fingerprint: row.get(11)?,
        last_test_ok: last_test_ok.map(|v| v != 0),
        last_test_at: row.get(13)?,
        last_test_message: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn open_conn(path: &Path) -> Result<Connection, rusqlite::Error> {
    let started = Instant::now();
    loop {
        match try_open_conn(path) {
            Ok(conn) => return Ok(conn),
            Err(err) if sqlite_is_busy(&err) && started.elapsed() < BUSY_WAIT => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }
}

fn try_open_conn(path: &Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_WAIT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", BUSY_WAIT.as_millis() as i64)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS devices (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            token_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_seen_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS hosts (
            id TEXT PRIMARY KEY,
            label TEXT NOT NULL,
            token_hash TEXT NOT NULL,
            state TEXT NOT NULL,
            last_seen_at TEXT,
            node_version TEXT,
            cli_json TEXT NOT NULL DEFAULT '[]',
            capabilities_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL,
            transport TEXT NOT NULL DEFAULT 'outbound-wss',
            labels_json TEXT NOT NULL DEFAULT '[]',
            herdr_json TEXT,
            resources_json TEXT,
            max_instances INTEGER NOT NULL DEFAULT 8,
            hostname TEXT
        );
        CREATE TABLE IF NOT EXISTS ssh_hosts (
            host_id TEXT PRIMARY KEY REFERENCES hosts(id) ON DELETE CASCADE,
            target TEXT NOT NULL UNIQUE,
            policy_json TEXT NOT NULL,
            last_error TEXT
        );
        CREATE TABLE IF NOT EXISTS instances (
            id TEXT PRIMARY KEY,
            host_id TEXT NOT NULL,
            workspace_id TEXT,
            kind TEXT NOT NULL,
            driver TEXT NOT NULL,
            lifecycle TEXT NOT NULL,
            activity TEXT NOT NULL,
            connectivity TEXT NOT NULL,
            title TEXT,
            journal_id TEXT NOT NULL,
            durable_seq INTEGER NOT NULL DEFAULT 0,
            spec_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_error TEXT
        );
        CREATE TABLE IF NOT EXISTS commands (
            id TEXT PRIMARY KEY,
            instance_id TEXT,
            host_id TEXT NOT NULL,
            operation TEXT NOT NULL,
            state TEXT NOT NULL,
            resolution TEXT NOT NULL,
            forwarded INTEGER NOT NULL DEFAULT 0,
            payload_json TEXT NOT NULL,
            idempotency_key TEXT UNIQUE,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS journal (
            instance_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            event_id TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            observed_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, seq)
        );
        CREATE TABLE IF NOT EXISTS fleets (
            id TEXT PRIMARY KEY,
            spec_json TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS fleet_members (
            fleet_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            PRIMARY KEY (fleet_id, instance_id)
        );
        CREATE TABLE IF NOT EXISTS pair_codes (
            code_hash TEXT PRIMARY KEY,
            created_by TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            used INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS interactions (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            state TEXT NOT NULL,
            blocking INTEGER NOT NULL DEFAULT 1,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS interactions_instance ON interactions(instance_id);
        CREATE INDEX IF NOT EXISTS interactions_host_state ON interactions(host_id, state);
        CREATE TABLE IF NOT EXISTS provider_profiles (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            base_url TEXT NOT NULL DEFAULT '',
            models_json TEXT NOT NULL DEFAULT '[]',
            default_model TEXT,
            headers_json TEXT NOT NULL DEFAULT '{}',
            is_default INTEGER NOT NULL DEFAULT 0,
            revision INTEGER NOT NULL DEFAULT 1,
            secret_name TEXT,
            secret_last4 TEXT,
            secret_fingerprint TEXT,
            last_test_ok INTEGER,
            last_test_at TEXT,
            last_test_message TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        ",
    )?;
    ensure_column(&conn, "hosts", "labels_json", "TEXT NOT NULL DEFAULT '[]'")?;
    ensure_column(&conn, "hosts", "herdr_json", "TEXT")?;
    ensure_column(&conn, "hosts", "resources_json", "TEXT")?;
    ensure_column(
        &conn,
        "hosts",
        "max_instances",
        "INTEGER NOT NULL DEFAULT 8",
    )?;
    ensure_column(&conn, "hosts", "hostname", "TEXT")?;
    ensure_column(&conn, "instances", "last_error", "TEXT")?;
    ensure_column(&conn, "hosts", "offline_since", "TEXT")?;
    ensure_column(&conn, "devices", "token_prefix", "TEXT")?;
    ensure_column(&conn, "hosts", "token_prefix", "TEXT")?;
    ensure_column(&conn, "pair_codes", "code_prefix", "TEXT")?;
    ensure_column(
        &conn,
        "pair_codes",
        "failed_attempts",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS devices_token_prefix ON devices(token_prefix) WHERE token_prefix IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS hosts_token_prefix ON hosts(token_prefix) WHERE token_prefix IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS pair_codes_prefix ON pair_codes(code_prefix) WHERE code_prefix IS NOT NULL;")?;
    dedup_duplicate_hosts(&conn)?;
    Ok(conn)
}

fn apply_instance_projection(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
    seq: i64,
    now: &str,
) -> Result<(), StoreError> {
    let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
    let payload = event.get("payload").cloned().unwrap_or(Value::Null);
    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let mut lifecycle: Option<&str> = None;
    let mut last_error: Option<String> = None;
    if kind == "lifecycle"
        && payload_type == "entity"
        && payload.get("entityType").and_then(Value::as_str) == Some("instance")
    {
        if payload.get("state").and_then(Value::as_str) == Some("failed") {
            lifecycle = Some("failed");
            last_error = payload
                .get("reasonCode")
                .and_then(Value::as_str)
                .map(str::to_string);
        } else if payload.get("state").and_then(Value::as_str) == Some("ready") {
            lifecycle = Some("ready");
        } else if payload.get("state").and_then(Value::as_str) == Some("exited") {
            lifecycle = Some("exited");
        }
        if let Some(entity_error) = payload.pointer("/entity/lastError").and_then(Value::as_str) {
            last_error = Some(entity_error.to_string());
        }
    }
    if kind == "lifecycle" && payload_type == "native" {
        let native_name = payload
            .get("nativeName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let severity = payload
            .get("severity")
            .and_then(Value::as_str)
            .unwrap_or("");
        let failed = severity == "error"
            || native_name.contains("error")
            || native_name == "exit"
            || native_name.contains("gone")
            || native_name.contains("agent_not_ready")
            || native_name.contains("shell");
        if failed {
            lifecycle = Some("failed");
            last_error = payload
                .pointer("/relatedIds/lastError")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    payload
                        .pointer("/status/value")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
        }
    }
    if let Some(lifecycle) = lifecycle {
        conn.execute(
            "UPDATE instances SET durable_seq = ?1, lifecycle = ?2,
                    last_error = COALESCE(?3, last_error), updated_at = ?4
             WHERE id = ?5",
            params![seq, lifecycle, last_error, now, instance_id],
        )?;
    } else {
        conn.execute(
            "UPDATE instances SET durable_seq = ?1, updated_at = ?2 WHERE id = ?3",
            params![seq, now, instance_id],
        )?;
    }
    Ok(())
}

fn apply_command_projection(
    conn: &Connection,
    host_id: &str,
    instance_id: &str,
    event: &Value,
    now: &str,
) -> Result<(), StoreError> {
    if event.get("kind").and_then(Value::as_str) != Some("lifecycle") {
        return Ok(());
    }
    let Some(payload) = event.get("payload") else {
        return Ok(());
    };
    if payload.get("type").and_then(Value::as_str) != Some("entity")
        || payload.get("entityType").and_then(Value::as_str) != Some("command")
    {
        return Ok(());
    }
    let Some(command_id) = payload
        .pointer("/entity/commandId")
        .and_then(Value::as_str)
        .or_else(|| payload.get("entityId").and_then(Value::as_str))
    else {
        return Ok(());
    };
    let Some(command) = load_command(conn, command_id)? else {
        return Ok(());
    };
    if command.host_id != host_id || command.instance_id.as_deref() != Some(instance_id) {
        return Err(StoreError::Id(
            "command lifecycle belongs to another host or instance".into(),
        ));
    }
    let state = payload.get("state").and_then(Value::as_str).unwrap_or("");
    if let Some(entity_state) = payload.pointer("/entity/state").and_then(Value::as_str)
        && entity_state != state
    {
        return Err(StoreError::Id(
            "command lifecycle state does not match its entity".into(),
        ));
    }
    match state {
        "accepted" => {
            conn.execute(
                "UPDATE commands SET state = 'accepted', resolution = 'clear', updated_at = ?1
                 WHERE id = ?2 AND state = 'queued'",
                params![now, command_id],
            )?;
        }
        "settled" => {
            conn.execute(
                "UPDATE commands SET state = 'settled', resolution = 'clear', updated_at = ?1
                 WHERE id = ?2 AND state != 'settled'",
                params![now, command_id],
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    name: &str,
    decl: &str,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|col| col == name);
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {decl}"), [])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn host_lost_obeys_offline_grace_and_reconnect_and_preserves_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let host = new_id("hst").unwrap();
        store
            .authenticate_host(
                HostAuthRequest {
                    presented: "bootstrap".into(),
                    bootstrap: "bootstrap".into(),
                    hello_host_id: Some(host.clone()),
                    label: Some("reclaim-test".into()),
                    node_version: None,
                },
                |_, _| false,
                |_| Ok("test-hash".into()),
            )
            .await
            .unwrap();
        let instance = store
            .insert_instance(
                host.clone(),
                None,
                "claude".into(),
                "generic-pty".into(),
                None,
                json!({}),
            )
            .await
            .unwrap();
        assert_eq!(
            store.expire_lost_hosts(0).await.unwrap(),
            0,
            "online hosts never expire"
        );
        store.mark_host_offline(host.clone()).await.unwrap();
        assert_eq!(
            store.expire_lost_hosts(600_000).await.unwrap(),
            0,
            "ten minute grace"
        );
        store
            .apply_inventory(host.clone(), Default::default(), None)
            .await
            .unwrap();
        assert_eq!(
            store.expire_lost_hosts(0).await.unwrap(),
            0,
            "reconnect clears offline timer"
        );
        store.mark_host_offline(host).await.unwrap();
        assert_eq!(store.expire_lost_hosts(0).await.unwrap(), 1);
        assert_eq!(
            store.expire_lost_hosts(0).await.unwrap(),
            0,
            "idempotent sweep"
        );
        let exited = store
            .get_instance(instance.instance_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exited.lifecycle, "exited");
        assert_eq!(exited.last_error.as_deref(), Some("host-lost"));
        assert_eq!(
            store.list_instances(None).await.unwrap().len(),
            1,
            "history is retained"
        );
    }

    #[tokio::test]
    async fn journal_settles_commands_while_connectivity_follows_the_host_link() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = Store::open(dir.path()).expect("store");
        let host_id = new_id("hst").expect("host id");
        let outcome = store
            .authenticate_host(
                HostAuthRequest {
                    presented: "bootstrap".into(),
                    bootstrap: "bootstrap".into(),
                    hello_host_id: Some(host_id.clone()),
                    label: Some("slow-node".into()),
                    node_version: Some("test".into()),
                },
                |_, _| false,
                |_| Ok("test-hash".into()),
            )
            .await
            .expect("host enroll");
        assert!(matches!(outcome, HostAuthOutcome::Authenticated { .. }));

        let instance = store
            .insert_instance(
                host_id.clone(),
                None,
                "claude".into(),
                "generic-pty".into(),
                None,
                json!({}),
            )
            .await
            .expect("instance");
        assert_eq!(instance.connectivity, "connected");
        let (command, _) = store
            .queue_command(
                None,
                Some(instance.instance_id.clone()),
                host_id.clone(),
                "instance.create".into(),
                json!({"instanceId": instance.instance_id}),
                None,
            )
            .await
            .expect("command");
        store
            .mark_forward_intent(command.command_id.clone())
            .await
            .expect("forward intent");

        for state in ["accepted", "settled"] {
            store
                .append_journal(
                    host_id.clone(),
                    instance.instance_id.clone(),
                    None,
                    json!({
                        "kind": "lifecycle",
                        "payload": {
                            "type": "entity",
                            "entityType": "command",
                            "entityId": command.command_id,
                            "state": state,
                            "entity": {
                                "commandId": command.command_id,
                                "state": state
                            }
                        }
                    }),
                )
                .await
                .expect("command lifecycle");
        }
        let projected = store
            .get_command(command.command_id.clone())
            .await
            .expect("command query")
            .expect("command row");
        assert_eq!(projected.state, "settled");
        assert_eq!(projected.resolution, "clear");

        store
            .append_journal(
                host_id.clone(),
                instance.instance_id.clone(),
                None,
                json!({
                    "kind": "lifecycle",
                    "payload": {
                        "type": "entity",
                        "entityType": "instance",
                        "entityId": instance.instance_id,
                        "state": "failed",
                        "reasonCode": "native-start-failed",
                        "entity": {"lastError": "native-start-failed"}
                    }
                }),
            )
            .await
            .expect("instance failure");
        let failed = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("instance query")
            .expect("instance row");
        assert_eq!(failed.lifecycle, "failed");
        assert_eq!(failed.connectivity, "connected");

        store
            .mark_host_offline(host_id.clone())
            .await
            .expect("offline");
        let offline = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("instance query")
            .expect("instance row");
        assert_eq!(offline.connectivity, "disconnected");

        store
            .apply_inventory(
                host_id,
                crate::inventory::HostInventoryUpdate::default(),
                None,
            )
            .await
            .expect("online");
        let online = store
            .get_instance(instance.instance_id)
            .await
            .expect("instance query")
            .expect("instance row");
        assert_eq!(online.connectivity, "connected");
    }

    #[tokio::test]
    async fn existing_host_requires_its_own_token() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = Store::open(dir.path()).expect("store");
        let host_id = new_id("hst").expect("host id");
        let first = store
            .authenticate_host(
                HostAuthRequest {
                    presented: "bootstrap".into(),
                    bootstrap: "bootstrap".into(),
                    hello_host_id: Some(host_id.clone()),
                    label: Some("local-development".into()),
                    node_version: Some("1".into()),
                },
                |_, _| false,
                |token| Ok(format!("hash:{token}")),
            )
            .await
            .expect("first enroll");
        let HostAuthOutcome::Authenticated {
            host,
            node_token: Some(node_token),
        } = first
        else {
            panic!("first enroll must insert a host token");
        };
        assert_eq!(host.host_id, host_id);
        store
            .mark_host_offline(host_id.clone())
            .await
            .expect("offline");
        let second = store
            .authenticate_host(
                HostAuthRequest {
                    presented: "bootstrap".into(),
                    bootstrap: "bootstrap".into(),
                    hello_host_id: Some(host_id.clone()),
                    label: Some("local-development".into()),
                    node_version: Some("2".into()),
                },
                |_, _| false,
                |_| Ok("hash-2".into()),
            )
            .await
            .expect("reannounce");
        assert!(matches!(second, HostAuthOutcome::Rejected));
        let existing = store
            .get_host(host_id.clone())
            .await
            .expect("query")
            .expect("host");
        assert!(
            !existing.online,
            "rejected enrollment must not change liveness"
        );
        assert_eq!(existing.node_version.as_deref(), Some("1"));
        let authenticated = store
            .authenticate_host(
                HostAuthRequest {
                    presented: node_token,
                    bootstrap: "bootstrap".into(),
                    // A host token authenticates its owner, regardless of a
                    // different identity claimed in the hello payload.
                    hello_host_id: Some(new_id("hst").expect("other host")),
                    label: Some("attacker label".into()),
                    node_version: Some("2".into()),
                },
                |token, hash| hash == format!("hash:{token}"),
                |_| panic!("reconnect must not mint a new token"),
            )
            .await
            .expect("host token reconnect");
        let HostAuthOutcome::Authenticated {
            host,
            node_token: None,
        } = authenticated
        else {
            panic!("reannounce must update without inserting");
        };
        assert_eq!(host.host_id, host_id);
        assert_eq!(host.node_version.as_deref(), Some("2"));
        assert!(host.online);
        let listed = store.list_hosts().await.expect("list");
        assert_eq!(listed.len(), 1, "{listed:?}");
    }

    #[tokio::test]
    async fn duplicate_label_hosts_merge_on_reopen() {
        let dir = tempfile::tempdir().expect("data dir");
        let host_a = new_id("hst").expect("a");
        let host_b = new_id("hst").expect("b");
        {
            let store = Store::open(dir.path()).expect("store");
            enroll_labeled(&store, host_a.clone(), "dup-node").await;
            enroll_labeled(&store, host_b.clone(), "dup-node").await;
            store
                .insert_instance(
                    host_a.clone(),
                    None,
                    "claude".into(),
                    "claude-print".into(),
                    Some("kept".into()),
                    json!({}),
                )
                .await
                .expect("instance");
            let listed = store.list_hosts().await.expect("list");
            assert_eq!(listed.len(), 2);
        }
        let store = Store::open(dir.path()).expect("reopen");
        let listed = store.list_hosts().await.expect("deduped");
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].label, "dup-node");
        assert_eq!(listed[0].instance_count, 1);
        let instances = store.list_instances(None).await.expect("instances");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].host_id, listed[0].host_id);
    }

    #[tokio::test]
    async fn instance_configure_is_queued_and_persisted_on_the_instance() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let host = new_id("hst").unwrap();
        store
            .authenticate_host(
                HostAuthRequest {
                    presented: "bootstrap".into(),
                    bootstrap: "bootstrap".into(),
                    hello_host_id: Some(host.clone()),
                    label: Some("configure-test".into()),
                    node_version: None,
                },
                |_, _| false,
                |_| Ok("test-hash".into()),
            )
            .await
            .unwrap();
        let instance = store
            .insert_instance(
                host.clone(),
                None,
                "claude".into(),
                "claude-print".into(),
                Some("configure".into()),
                json!({ "model": "haiku" }),
            )
            .await
            .unwrap();
        let payload = json!({
            "instanceId": instance.instance_id,
            "model": "opus",
            "effort": { "index": 3, "name": "ultracode", "kind": "claude" }
        });
        let (command, created) = store
            .queue_command(
                None,
                Some(instance.instance_id.clone()),
                host,
                "instance.configure".into(),
                payload.clone(),
                None,
            )
            .await
            .unwrap();
        assert!(created);
        assert_eq!(command.operation, "instance.configure");
        assert_eq!(command.state, "queued");
        assert_eq!(command.payload["effort"]["name"], json!("ultracode"));
        assert_eq!(command.payload["effort"]["index"], json!(3));
        let patched = store
            .patch_instance_configure(instance.instance_id.clone(), payload)
            .await
            .unwrap();
        assert_eq!(patched.model.as_deref(), Some("opus"));
        assert_eq!(patched.effort_name.as_deref(), Some("ultracode"));
        assert_eq!(patched.effort_index, Some(3));
        let reloaded = store
            .get_instance(instance.instance_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.model.as_deref(), Some("opus"));
        assert_eq!(reloaded.effort_name.as_deref(), Some("ultracode"));
        assert_eq!(reloaded.effort_index, Some(3));
        let listed = store.list_instances(None).await.unwrap();
        assert_eq!(listed[0].effort_name.as_deref(), Some("ultracode"));
    }

    async fn enroll_labeled(store: &Store, host_id: String, label: &str) {
        let outcome = store
            .authenticate_host(
                HostAuthRequest {
                    presented: "bootstrap".into(),
                    bootstrap: "bootstrap".into(),
                    hello_host_id: Some(host_id),
                    label: Some(label.to_owned()),
                    node_version: Some("test".into()),
                },
                |_, _| false,
                |secret| Ok(format!("hash-{secret}")),
            )
            .await
            .expect("enroll");
        assert!(matches!(outcome, HostAuthOutcome::Authenticated { .. }));
    }
}

fn touch_host_online(
    conn: &Connection,
    host_id: &str,
    node_version: &Option<String>,
) -> Result<HostRecord, StoreError> {
    let now = now_rfc3339();
    conn.execute(
        "UPDATE hosts SET state = CASE WHEN EXISTS (SELECT 1 FROM ssh_hosts WHERE host_id = hosts.id) THEN state ELSE 'online' END, offline_since = NULL, last_seen_at = ?1, node_version = COALESCE(?2, node_version)
         WHERE id = ?3 AND state != 'retired'",
        params![now, node_version, host_id],
    )?;
    load_host(conn, host_id)?.ok_or_else(|| StoreError::Id("host vanished after auth".into()))
}

struct HostDedupRow {
    id: String,
    state: String,
    last_seen: String,
    created: String,
}

fn dedup_duplicate_hosts(conn: &Connection) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, label, IFNULL(hostname, ''), state, IFNULL(last_seen_at, ''), created_at
         FROM hosts WHERE NOT EXISTS (SELECT 1 FROM ssh_hosts WHERE host_id = hosts.id)",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut groups: BTreeMap<(String, String), Vec<HostDedupRow>> = BTreeMap::new();
    for row in rows {
        let (id, label, hostname, state, last_seen, created) = row?;
        groups
            .entry((label, hostname))
            .or_default()
            .push(HostDedupRow {
                id,
                state,
                last_seen,
                created,
            });
    }
    drop(stmt);
    for members in groups.into_values() {
        if members.len() < 2 {
            continue;
        }
        let mut members = members;
        members.sort_by(|a, b| {
            let a_online = a.state == "online";
            let b_online = b.state == "online";
            b_online
                .cmp(&a_online)
                .then(b.last_seen.cmp(&a.last_seen))
                .then(b.created.cmp(&a.created))
                .then(a.id.cmp(&b.id))
        });
        let survivor = members[0].id.clone();
        for row in members.into_iter().skip(1) {
            conn.execute(
                "UPDATE instances SET host_id = ?1 WHERE host_id = ?2",
                params![&survivor, &row.id],
            )?;
            conn.execute(
                "UPDATE commands SET host_id = ?1 WHERE host_id = ?2",
                params![&survivor, &row.id],
            )?;
            conn.execute(
                "UPDATE fleet_members SET host_id = ?1 WHERE host_id = ?2",
                params![&survivor, &row.id],
            )?;
            conn.execute(
                "UPDATE interactions SET host_id = ?1 WHERE host_id = ?2",
                params![&survivor, &row.id],
            )?;
            conn.execute("DELETE FROM hosts WHERE id = ?1", params![&row.id])?;
        }
    }
    Ok(())
}

pub(crate) fn load_host(conn: &Connection, id: &str) -> Result<Option<HostRecord>, StoreError> {
    let row = conn
        .query_row(
            "SELECT id, label, state, last_seen_at, node_version, cli_json, capabilities_json, transport,
                    labels_json, herdr_json, resources_json, max_instances, hostname
             FROM hosts WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, Option<String>>(12)?,
                ))
            },
        )
        .optional()?;
    let Some((
        host_id,
        label,
        state,
        last_seen_at,
        node_version,
        cli,
        caps,
        transport,
        labels_json,
        herdr_json,
        resources_json,
        max_instances,
        hostname,
    )) = row
    else {
        return Ok(None);
    };
    let instance_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM instances WHERE host_id = ?1",
        params![host_id],
        |row| row.get(0),
    )?;
    let labels: Vec<String> = serde_json::from_str(&labels_json).unwrap_or_default();
    let managed = conn
        .query_row(
            "SELECT target, policy_json, last_error FROM ssh_hosts WHERE host_id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let (ssh, last_error) = match managed {
        Some((target, policy, error)) => (
            Some(
                json!({"target": target, "workspaceRoot": format!("/tmp/remuda-ssh-{id}/workspace"), "remudaBinaryPolicy": serde_json::from_str::<Value>(&policy)?}),
            ),
            error,
        ),
        None => (None, None),
    };
    Ok(Some(HostRecord {
        ssh,
        last_error,
        host_id,
        label,
        online: state == "online",
        state,
        last_seen_at,
        node_version,
        cli: serde_json::from_str(&cli).unwrap_or(json!([])),
        capabilities: serde_json::from_str(&caps).unwrap_or(json!({})),
        instance_count,
        transport,
        labels,
        herdr: herdr_json.and_then(|raw| serde_json::from_str(&raw).ok()),
        resources: resources_json.and_then(|raw| serde_json::from_str(&raw).ok()),
        max_instances,
        hostname,
    }))
}

fn load_instance(conn: &Connection, id: &str) -> Result<Option<InstanceRecord>, StoreError> {
    conn.query_row(
        "SELECT id, host_id, workspace_id, kind, driver, lifecycle, activity, connectivity,
                title, journal_id, durable_seq, created_at, updated_at, spec_json, last_error
         FROM instances WHERE id = ?1",
        params![id],
        |row| {
            let durable: i64 = row.get(10)?;
            let workspace_id: Option<String> = row.get(2)?;
            let title: Option<String> = row.get(8)?;
            let spec_raw: String = row.get(13)?;
            let spec: Value = serde_json::from_str(&spec_raw).unwrap_or(json!({}));
            let cwd = spec
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| workspace_id.clone());
            let name = spec
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| title.clone());
            let delegation = spec
                .get("delegation")
                .and_then(Value::as_str)
                .map(str::to_string);
            let provider_profile_id = spec
                .get("providerProfileId")
                .and_then(Value::as_str)
                .map(str::to_string);
            let model = spec
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            let effort = spec.get("effort");
            let effort_name = effort
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    spec.get("effortName")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            let effort_index = effort
                .and_then(|value| value.get("index"))
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .or_else(|| {
                    spec.get("effortIndex")
                        .and_then(Value::as_u64)
                        .map(|n| n as u32)
                });
            Ok(InstanceRecord {
                instance_id: row.get(0)?,
                host_id: row.get(1)?,
                workspace_id,
                kind: row.get(3)?,
                driver: row.get(4)?,
                lifecycle: row.get(5)?,
                activity: row.get(6)?,
                connectivity: row.get(7)?,
                title,
                name,
                cwd,
                delegation,
                provider_profile_id,
                model,
                effort_name,
                effort_index,
                journal_id: row.get(9)?,
                durable_seq: durable.to_string(),
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
                last_error: row.get(14)?,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

fn load_journal_row(
    conn: &Connection,
    instance_id: &str,
    seq: i64,
) -> Result<Option<JournalRecord>, StoreError> {
    conn.query_row(
        "SELECT instance_id, seq, event_id, payload_json, observed_at
         FROM journal WHERE instance_id = ?1 AND seq = ?2",
        params![instance_id, seq],
        |row| {
            let payload: String = row.get(3)?;
            let event: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            Ok(JournalRecord {
                instance_id: row.get(0)?,
                seq: row.get(1)?,
                event_id: row.get(2)?,
                event,
                observed_at: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

fn load_command(conn: &Connection, id: &str) -> Result<Option<CommandRecord>, StoreError> {
    conn.query_row(
        "SELECT id, instance_id, host_id, operation, state, resolution, forwarded,
                payload_json, idempotency_key, created_at, updated_at
         FROM commands WHERE id = ?1",
        params![id],
        command_from_row,
    )
    .optional()
    .map_err(StoreError::from)
}

fn load_command_by_key(conn: &Connection, key: &str) -> Result<Option<CommandRecord>, StoreError> {
    conn.query_row(
        "SELECT id, instance_id, host_id, operation, state, resolution, forwarded,
                payload_json, idempotency_key, created_at, updated_at
         FROM commands WHERE idempotency_key = ?1",
        params![key],
        command_from_row,
    )
    .optional()
    .map_err(StoreError::from)
}

fn interaction_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InteractionRecord> {
    let payload: String = row.get(6)?;
    let blocking: i64 = row.get(5)?;
    Ok(InteractionRecord {
        interaction_id: row.get(0)?,
        instance_id: row.get(1)?,
        host_id: row.get(2)?,
        kind: row.get(3)?,
        state: row.get(4)?,
        blocking: blocking != 0,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn load_interaction(conn: &Connection, id: &str) -> Result<Option<InteractionRecord>, StoreError> {
    conn.query_row(
        "SELECT id, instance_id, host_id, kind, state, blocking, payload_json, created_at, updated_at
         FROM interactions WHERE id = ?1",
        params![id],
        interaction_from_row,
    )
    .optional()
    .map_err(StoreError::from)
}

fn apply_interaction_event(
    conn: &Connection,
    host_id: &str,
    instance_id: &str,
    event: &Value,
) -> Result<(), StoreError> {
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .or_else(|| event.get("subtype").and_then(Value::as_str))
        .unwrap_or("");
    if event.pointer("/payload/entityType").and_then(Value::as_str) == Some("interaction") {
        if let Some(entity) = event.pointer("/payload/entity")
            && let (Some(id), Some(state)) = (
                entity.get("id").and_then(Value::as_str),
                entity.get("state").and_then(Value::as_str),
            )
        {
            conn.execute("UPDATE interactions SET state = ?1, blocking = 0, payload_json = ?2, updated_at = ?3 WHERE id = ?4",
                params![state, event.to_string(), now_rfc3339(), id])?;
        }
        return Ok(());
    }
    let id = event
        .get("interactionId")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .pointer("/payload/interactionId")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            event
                .pointer("/payload/interaction/id")
                .and_then(Value::as_str)
        });
    let Some(id) = id.filter(|id| !id.is_empty()) else {
        return Ok(());
    };
    let now = now_rfc3339();
    if kind == "interaction.requested" || kind == "interactionRequested" {
        let ikind = event
            .get("interactionKind")
            .and_then(Value::as_str)
            .or_else(|| event.pointer("/payload/kind").and_then(Value::as_str))
            .or_else(|| {
                event
                    .pointer("/payload/interaction/kind")
                    .and_then(Value::as_str)
            })
            .unwrap_or("permission");
        conn.execute(
            "INSERT INTO interactions
                (id, instance_id, host_id, kind, state, blocking, payload_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 1, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET
                payload_json = excluded.payload_json,
                updated_at = excluded.updated_at,
                state = CASE WHEN interactions.state = 'pending' THEN 'pending' ELSE interactions.state END",
            params![id, instance_id, host_id, ikind, event.to_string(), now],
        )?;
        conn.execute(
            "UPDATE instances SET activity = 'blocked', updated_at = ?1 WHERE id = ?2",
            params![now, instance_id],
        )?;
    } else if kind == "interaction.answered"
        || kind == "interactionAnswered"
        || kind == "interaction.expired"
        || kind == "interactionExpired"
    {
        let state = if kind.contains("expired") {
            "expired"
        } else {
            "answer-committed"
        };
        conn.execute(
            "UPDATE interactions SET state = ?1, updated_at = ?2 WHERE id = ?3 AND state = 'pending'",
            params![state, now, id],
        )?;
    }
    Ok(())
}

fn knowledge_value(value: Option<&Value>) -> Option<&str> {
    let value = value?;
    value
        .as_str()
        .or_else(|| value.get("value").and_then(Value::as_str))
}

fn lifecycle_rank(state: &str) -> i32 {
    match state {
        "requested" => 0,
        "preparing" | "starting" => 1,
        "ready" | "running" => 2,
        "closing" => 3,
        "exited" | "failed" => 4,
        _ => 0,
    }
}

fn normalize_lifecycle(state: &str) -> Option<&'static str> {
    match state {
        "requested" => Some("requested"),
        "preparing" | "starting" => Some("starting"),
        "ready" | "running" => Some("running"),
        "closing" => Some("closing"),
        "exited" => Some("exited"),
        "failed" => Some("failed"),
        _ => None,
    }
}

fn normalize_activity(status: &str) -> Option<&'static str> {
    match status {
        "idle" | "done" => Some("idle"),
        "working" => Some("working"),
        "blocked" | "waiting-interaction" => Some("blocked"),
        "draining" => Some("draining"),
        "unknown" => Some("unknown"),
        _ => None,
    }
}

/// Derive Hub lifecycle/activity from a mirrored Node observation.
///
/// `activity=idle` is only set from a Node/herdr idle observation, never as a
/// create default. Start-failure observations (`native-driver-start-failed`,
/// entity `failed`) mark `lifecycle=failed`.
fn derive_instance_state(event: &Value) -> (Option<&'static str>, Option<&'static str>) {
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .or_else(|| event.get("subtype").and_then(Value::as_str))
        .unwrap_or("");
    let payload = event.get("payload").unwrap_or(event);
    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let reason = payload
        .get("reasonCode")
        .and_then(Value::as_str)
        .unwrap_or("");
    let native_name = payload
        .get("nativeName")
        .and_then(Value::as_str)
        .unwrap_or("");
    let entity_state = payload
        .get("state")
        .and_then(Value::as_str)
        .or_else(|| event.get("state").and_then(Value::as_str));
    let status = knowledge_value(payload.get("status"))
        .or_else(|| event.get("activity").and_then(Value::as_str));

    let start_failed = reason == "native-driver-start-failed"
        || native_name == "native-driver-start-failed"
        || native_name.contains("start-fail")
        || status.is_some_and(|s| s == "failed" || s == "error")
        || entity_state == Some("failed");
    if start_failed && (kind == "lifecycle" || payload_type == "native" || payload_type == "entity")
    {
        return (Some("failed"), None);
    }

    if kind == "interaction.requested" || kind == "interactionRequested" {
        return (Some("running"), Some("blocked"));
    }

    let mut lifecycle = None;
    let mut activity = None;
    if let Some(state) = entity_state {
        lifecycle = normalize_lifecycle(state);
    }
    let herdr_idle_proof =
        native_name == "agent_status" || native_name == "session" || payload_type == "native";
    if herdr_idle_proof && let Some(status) = status {
        match status {
            "starting" | "started" => {
                lifecycle = Some(if status == "starting" {
                    "starting"
                } else {
                    "running"
                });
            }
            "idle" | "done" | "working" | "blocked" | "waiting-interaction" => {
                lifecycle = Some("running");
                activity = normalize_activity(status);
            }
            "exited" => lifecycle = Some("exited"),
            "failed" | "error" => lifecycle = Some("failed"),
            _ => {}
        }
    }
    (lifecycle, activity)
}

fn apply_instance_lifecycle(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
) -> Result<(), StoreError> {
    let (next_life, next_act) = derive_instance_state(event);
    if next_life.is_none() && next_act.is_none() {
        return Ok(());
    }
    let Some(current) = load_instance(conn, instance_id)? else {
        return Ok(());
    };
    let now = now_rfc3339();
    let lifecycle = match next_life {
        Some(next) if lifecycle_rank(next) >= lifecycle_rank(&current.lifecycle) => next,
        _ => current.lifecycle.as_str(),
    };
    let activity = next_act.unwrap_or(current.activity.as_str());
    conn.execute(
        "UPDATE instances SET lifecycle = ?1, activity = ?2, updated_at = ?3 WHERE id = ?4",
        params![lifecycle, activity, now, instance_id],
    )?;
    Ok(())
}

fn command_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRecord> {
    let payload: String = row.get(7)?;
    let forwarded: i64 = row.get(6)?;
    Ok(CommandRecord {
        command_id: row.get(0)?,
        instance_id: row.get(1)?,
        host_id: row.get(2)?,
        operation: row.get(3)?,
        state: row.get(4)?,
        resolution: row.get(5)?,
        forwarded: forwarded != 0,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
        idempotency_key: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[cfg(test)]
mod derive_tests {
    use super::derive_instance_state;
    use serde_json::json;

    #[test]
    fn create_default_is_not_idle() {
        let (life, act) = derive_instance_state(&json!({"kind": "message"}));
        assert_eq!(life, None);
        assert_eq!(act, None);
    }

    #[test]
    fn entity_ready_is_running_not_idle() {
        let (life, act) = derive_instance_state(&json!({
            "kind": "lifecycle",
            "payload": { "type": "entity", "state": "ready", "reasonCode": "driver-started" }
        }));
        assert_eq!(life, Some("running"));
        assert_eq!(act, None);
    }

    #[test]
    fn herdr_agent_status_idle_is_idle_proof() {
        let (life, act) = derive_instance_state(&json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "nativeName": "agent_status",
                "status": { "state": "known", "value": "idle" }
            }
        }));
        assert_eq!(life, Some("running"));
        assert_eq!(act, Some("idle"));
    }

    #[test]
    fn start_failure_marks_failed() {
        let (life, _) = derive_instance_state(&json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity",
                "state": "failed",
                "reasonCode": "native-driver-start-failed"
            }
        }));
        assert_eq!(life, Some("failed"));
    }
}
