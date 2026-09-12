//! Single-writer SQLite actor. Connections never cross `.await`.

use crate::config::{new_id, now_rfc3339};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::thread;
use thiserror::Error;
use tokio::sync::oneshot;

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

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// Handle to the Hub database writer.
#[derive(Clone)]
pub struct Store {
    tx: std::sync::mpsc::Sender<Job>,
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
    /// Journal id (`obj_…`).
    pub journal_id: String,
    /// Durable seq as decimal string.
    pub durable_seq: String,
    /// Create-time.
    pub created_at: String,
    /// Update-time.
    pub updated_at: String,
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

impl Store {
    /// Open (or create) `hub.sqlite` on a dedicated writer thread.
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(data_dir).map_err(|err| StoreError::Id(err.to_string()))?;
        let path = data_dir.join("hub.sqlite");
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        thread::Builder::new()
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
                    job(&mut conn);
                }
            })
            .map_err(|err| StoreError::Id(err.to_string()))?;
        Ok(Self { tx })
    }

    async fn run<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Box::new(move |conn| {
                let _ = tx.send(f(conn));
            }))
            .map_err(|_| StoreError::Closed)?;
        rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Insert a device token hash.
    pub async fn insert_device(
        &self,
        name: String,
        token_hash: String,
    ) -> Result<Device, StoreError> {
        self.run(move |conn| {
            let id = new_id("dev").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO devices (id, name, token_hash, created_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params![id, name, token_hash, now],
            )?;
            Ok(Device { id, name })
        })
        .await
    }

    /// Lookup a device by verifying `token` against stored hashes.
    pub async fn find_device_by_token<F>(
        &self,
        token: String,
        verify: F,
    ) -> Result<Option<Device>, StoreError>
    where
        F: Fn(&str, &str) -> bool + Send + 'static,
    {
        self.run(move |conn| {
            let mut stmt =
                conn.prepare("SELECT id, name, token_hash FROM devices ORDER BY created_at")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (id, name, hash) = row?;
                if verify(&token, &hash) {
                    let now = now_rfc3339();
                    conn.execute(
                        "UPDATE devices SET last_seen_at = ?1 WHERE id = ?2",
                        params![now, id],
                    )?;
                    return Ok(Some(Device { id, name }));
                }
            }
            Ok(None)
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
            let mut stmt = conn.prepare("SELECT id, token_hash FROM hosts")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, hash) = row?;
                if verify(&request.presented, &hash) {
                    let now = now_rfc3339();
                    conn.execute(
                        "UPDATE hosts SET state = 'online', last_seen_at = ?1, node_version = COALESCE(?2, node_version)
                         WHERE id = ?3",
                        params![now, request.node_version, id],
                    )?;
                    let host = load_host(conn, &id)?.ok_or_else(|| {
                        StoreError::Id("host vanished after auth".into())
                    })?;
                    return Ok(HostAuthOutcome::Authenticated {
                        host: Box::new(host),
                        node_token: None,
                    });
                }
            }
            if !crate::config::secret_eq(&request.presented, &request.bootstrap) {
                return Ok(HostAuthOutcome::Rejected);
            }
            let host_id = match request.hello_host_id {
                Some(id) => id,
                None => new_id("hst").map_err(|e| StoreError::Id(e.to_string()))?,
            };
            if load_host(conn, &host_id)?.is_some() {
                return Err(StoreError::Id(
                    "host already enrolled; present the host token".into(),
                ));
            }
            let node_token = crate::config::random_token();
            let token_hash = hash_new(&node_token)?;
            let now = now_rfc3339();
            let label = request.label.unwrap_or_else(|| host_id.clone());
            conn.execute(
                "INSERT INTO hosts
                    (id, label, token_hash, state, last_seen_at, node_version, cli_json, capabilities_json,
                     created_at, transport, labels_json, herdr_json, resources_json, max_instances, hostname)
                 VALUES (?1, ?2, ?3, 'online', ?4, ?5, '[]', '{}', ?4, 'outbound-wss', '[]', NULL, NULL, 8, NULL)",
                params![host_id, label, token_hash, now, request.node_version],
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

    /// Mark a host offline when its WS drops.
    pub async fn mark_host_offline(&self, host_id: String) -> Result<(), StoreError> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE hosts SET state = 'offline' WHERE id = ?1 AND state = 'online'",
                params![host_id],
            )?;
            Ok(())
        })
        .await
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
                "UPDATE hosts SET last_seen_at = ?1, state = 'online' WHERE id = ?2",
                params![now, host_id],
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
            let instance_id = new_id("ins").map_err(|e| StoreError::Id(e.to_string()))?;
            let journal_id = new_id("obj").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO instances
                    (id, host_id, workspace_id, kind, driver, lifecycle, activity, connectivity,
                     title, journal_id, durable_seq, spec_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'requested', 'idle', 'disconnected',
                         ?6, ?7, 0, ?8, ?9, ?9)",
                params![
                    instance_id,
                    host_id,
                    workspace_id,
                    kind,
                    driver,
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
                 VALUES (?1, ?2, NULL, 'claude', 'claude-print', 'ready', 'idle', 'connected',
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
                "UPDATE commands SET state = 'accepted', resolution = 'clear', updated_at = ?1 WHERE id = ?2",
                params![now, command_id],
            )?;
            load_command(conn, &command_id)?
                .ok_or_else(|| StoreError::Id("unknown command".into()))
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
    ) -> Result<(JournalRecord, bool), StoreError> {
        self.run(move |conn| {
            let inst = load_instance(conn, &instance_id)?
                .ok_or_else(|| StoreError::Id("unknown instance".into()))?;
            if inst.host_id != host_id {
                return Err(StoreError::Id("instance belongs to another host".into()));
            }
            let next = inst
                .durable_seq
                .parse::<i64>()
                .unwrap_or(0)
                + 1;
            let seq = seq.unwrap_or(next);
            if seq < next {
                let existing = load_journal_row(conn, &instance_id, seq)?.ok_or_else(|| {
                    StoreError::Id(format!("journal gap: expected {next}, got {seq}"))
                })?;
                return Ok((existing, false));
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
                .unwrap_or_else(|| {
                    new_id("evt").unwrap_or_else(|_| "evt_missing".into())
                });
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
            conn.execute(
                "UPDATE instances SET durable_seq = ?1, connectivity = 'connected', updated_at = ?2 WHERE id = ?3",
                params![seq, now, instance_id],
            )?;
            Ok((
                JournalRecord {
                    instance_id,
                    seq,
                    event_id,
                    event,
                    observed_at: now,
                },
                true,
            ))
        })
        .await
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
        created_by: String,
        expires_at: String,
    ) -> Result<(), StoreError> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO pair_codes (code_hash, created_by, expires_at, used)
                 VALUES (?1, ?2, ?3, 0)",
                params![code_hash, created_by, expires_at],
            )?;
            Ok(())
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
            let mut stmt = conn.prepare("SELECT code_hash, expires_at, used FROM pair_codes")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (hash, expires_at, used) = row?;
                if used != 0 || expires_at < now {
                    continue;
                }
                if verify(&presented, &hash) {
                    conn.execute(
                        "UPDATE pair_codes SET used = 1 WHERE code_hash = ?1",
                        params![hash],
                    )?;
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
    }
}

fn open_conn(path: &Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
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
            updated_at TEXT NOT NULL
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
    Ok(conn)
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

fn load_host(conn: &Connection, id: &str) -> Result<Option<HostRecord>, StoreError> {
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
    Ok(Some(HostRecord {
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
                title, journal_id, durable_seq, created_at, updated_at
         FROM instances WHERE id = ?1",
        params![id],
        |row| {
            let durable: i64 = row.get(10)?;
            Ok(InstanceRecord {
                instance_id: row.get(0)?,
                host_id: row.get(1)?,
                workspace_id: row.get(2)?,
                kind: row.get(3)?,
                driver: row.get(4)?,
                lifecycle: row.get(5)?,
                activity: row.get(6)?,
                connectivity: row.get(7)?,
                title: row.get(8)?,
                journal_id: row.get(9)?,
                durable_seq: durable.to_string(),
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
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
