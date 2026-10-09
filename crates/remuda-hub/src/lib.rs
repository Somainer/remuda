//! Hub authentication, host registry, routing, and embedded Web assets.
//!
//! The `remuda` composition root owns the `hub` subcommand. This crate exposes
//! [`serve`] / [`spawn`] so that CLI agent can call:
//!
//! ```text
//! remuda hub --data-dir … --listen 127.0.0.1:8080
//! ```

#![allow(missing_docs)] // handler types; public API is documented below.

pub mod agent_approvals;
mod agent_scope;
mod alerts;
mod api_relay;
mod attachments;
mod auth;
mod bot;
mod config;
mod devices;
mod error;
mod fleet;
mod gatequeue;
mod host_dirs;
mod host_files;
mod hosts;
mod http;
mod instances;
mod interactions;
/// Test-only seam for the D-051 delegated-decisions feature switch.
#[doc(hidden)]
pub use interactions::delegated_decisions_test_support;
/// Test-only seam for the D-057 continuation-resume race.
#[doc(hidden)]
pub use store::lineage_test_support;
mod inventory;
mod maintenance;
mod model_catalog;
/// Built-in model catalog revision (tests compare the served catalog against it).
pub use model_catalog::CATALOG_REVISION;
mod objects;
mod passkeys;
mod placement;
mod projects;
mod provider_models;
mod provider_resolve;
mod providers;
mod proxy;
mod push_http;
mod rate_limit;
mod registry;
pub mod ssh_hosts;
mod store;
mod store_tickets;
/// Managed-process supervision (`--managed`): parent-death watch and pid file.
pub mod supervise;
mod supply;
mod tasks;
mod transport;
mod tty;
mod usage_store;
mod web;
mod worker_watch;
mod workers;
mod workspaces;
mod ws;

use crate::alerts::{BlockedWatch, Followers};
use crate::auth::{
    BootstrapResolution, adopt_bootstrap_after_bind, persist_listen, resolve_bootstrap,
};
use crate::store::Store;
use crate::ws::Bus;
use axum::Router;
use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use remuda_driver::FileSecretStore;
use remuda_push::{OpenOptions, PushService};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub use agent_scope::instance_token;
pub use auth::{bootstrap_issued_at, persist_bootstrap, rotate_bootstrap};
pub use config::{
    BootstrapSource, DEFAULT_ATTACHMENT_MAX_BYTES, DEFAULT_BOOTSTRAP_TTL_HOURS,
    DEFAULT_COMMAND_ACCEPT_TIMEOUT_MS, DEFAULT_ENROLL_TOKEN_TTL_MINUTES, HubConfig,
    MIN_CREATE_SETTLE_TIMEOUT_MS,
};
pub use error::HubError;
pub use maintenance::migrate;
pub use transport::{
    CallGate, ConnectedNodes, GatedTransport, NodeTransport, StdioTransport, TransportKind,
    WssTransport,
};

/// Process-wide Hub state shared by HTTP and WS handlers.
#[derive(Clone)]
pub struct AppState {
    /// Frozen listen/auth config.
    pub config: Arc<HubConfig>,
    store: Store,
    /// Envelope-encrypted provider tokens under `data_dir/secrets`.
    secrets: Arc<FileSecretStore>,
    nodes: ConnectedNodes,
    bus: Bus,
    tty: crate::ws::TtyRelay,
    /// D-048 relay stream registry (per-Hub-process).
    api_relay: crate::api_relay::ApiRelay,
    push: Option<PushService>,
    followers: Followers,
    blocked: BlockedWatch,
    auth_limits: rate_limit::AuthRateLimits,
    agent_approvals: agent_approvals::AgentApprovals,
    /// Process-local, single-use WebAuthn ceremony challenges (D-030).
    challenges: passkeys::ChallengeStore,
    /// When the pinned-ref retention sweep last ran. Per-Hub rather than a
    /// process static so concurrent instances (tests) never starve each other.
    gate_ref_swept_at: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Round 6 item 3: test-only park points in the real unregister/task-bind
    /// handlers; empty (no-op) in production.
    pub(crate) race_barriers: crate::workspaces::RaceBarriers,
}

impl AppState {
    /// Raw per-chunk cap for D-048 relay streams, from the D-048 protocol
    /// default (`TransportLimits.apiChunkBytes`, 64 KiB).
    pub(crate) fn config_relay_chunk_bytes(&self) -> usize {
        remuda_protocol::default_api_chunk_bytes() as usize
    }
}

/// Test-only constructors for types private modules would otherwise hide
/// from integration tests. Not part of the supported API.
#[doc(hidden)]
pub mod store_test_support {
    use std::path::Path;
    use std::time::Duration;

    use crate::store::{CommandRecord, CommandSettlement, InstanceDelegation};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    pub use crate::store::{APPEND_CHUNK_MAX, JOURNAL_WINDOW_BYTES, JOURNAL_WINDOW_ROWS, Store};

    /// D-057 continuation-resume inputs/outcomes for the race suite.
    pub use crate::store::{ContinuationResumeRequest, ContinuationResumeResult};

    /// A leaf-worker delegation scoped to one project.
    pub fn leaf_delegation(project_id: &str) -> Result<InstanceDelegation, String> {
        let project = remuda_protocol::ProjectId::try_from(project_id.to_string())
            .map_err(|err| err.to_string())?;
        Ok(InstanceDelegation {
            role: Some(remuda_protocol::ROLE_WORKER.into()),
            scope: remuda_protocol::InstanceScope {
                project_ids: vec![project],
                ..Default::default()
            },
            grants: Vec::new(),
            task_id: None,
            enforce_tree: true,
            restart: None,
        })
    }

    /// Open a Hub DB and enroll one host, returning the store and its host id.
    ///
    /// The Store-direct equivalent of the `spawn` + fake-node WS dance, for
    /// tests that exercise the writer/reader split rather than HTTP.
    pub async fn open_with_host(dir: &Path, label: &str) -> anyhow::Result<(Store, String)> {
        use crate::store::{HostAuthOutcome, HostAuthRequest};

        let store = Store::open(dir)?;
        // Prefix-indexed tokens are 64 hex chars; derive a deterministic one.
        let mut plaintext: String = label.bytes().map(|b| format!("{b:02x}")).collect();
        if plaintext.len() < 64 {
            plaintext = format!("{plaintext:0<64}");
        }
        plaintext.truncate(64);
        let prefix = crate::auth::token_prefix(&plaintext).map(str::to_string);
        store
            .insert_enroll_token(
                plaintext.clone(),
                prefix,
                "hub-store-test".into(),
                "2099-01-01T00:00:00.000Z".into(),
            )
            .await?;
        let host_id = crate::config::new_id("hst")?;
        let outcome = store
            .authenticate_host(
                HostAuthRequest {
                    presented: plaintext,
                    hello_host_id: Some(host_id.clone()),
                    label: Some(label.into()),
                    node_version: None,
                },
                |presented, hash| presented == hash,
                |_| Ok("test-hash".into()),
            )
            .await?;
        anyhow::ensure!(
            matches!(outcome, HostAuthOutcome::Authenticated { .. }),
            "host enrollment rejected"
        );
        Ok((store, host_id))
    }

    /// Test fixture: settle an ensured instance as `exited` and bind it to a
    /// task, the shape the t-pool delete/retire guards read.
    pub async fn settle_exited_with_task(
        store: &Store,
        instance_id: &str,
        task_id: &str,
    ) -> anyhow::Result<()> {
        let instance_id = instance_id.to_string();
        let task_id = task_id.to_string();
        store
            .run_named("test.settle_exited_with_task", move |conn| {
                let now = crate::config::now_rfc3339();
                conn.execute(
                    "UPDATE instances
                        SET lifecycle = 'exited', activity = 'closed',
                            task_id = ?2, updated_at = ?3
                      WHERE id = ?1",
                    rusqlite::params![instance_id, task_id, now],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Test fixture: occupy one reader-pool connection for `sleep`, exactly
    /// like a long read landing on the pool. The pool has more than one
    /// connection, so other reads and the writer thread must not wait.
    pub async fn hold_reader(store: &Store, sleep: Duration) {
        store
            .read("test.hold_reader", move |_| {
                std::thread::sleep(sleep);
                Ok(())
            })
            .await
            .expect("reader hold");
    }

    /// Occupy the single WRITER connection for `sleep`, reproducing a long job
    /// that used to block every queued job. Drives the hub-store-1 evidence
    /// harness (`tests/hub_store_evidence.rs`, `--ignored`).
    pub async fn hold_writer(store: &Store, sleep: Duration) {
        store
            .run_named("test.hold_writer", move |_| {
                std::thread::sleep(sleep);
                Ok(())
            })
            .await
            .expect("writer hold");
    }

    /// Read + JSON-parse every journal row, reproducing the pre-hub-store-1
    /// unbounded `read_journal` (no LIMIT). Evidence harness only.
    pub async fn read_all_journal_parsed(store: &Store, instance_id: &str) -> usize {
        let instance_id = instance_id.to_owned();
        store
            .read("test.read_all_journal", move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT payload_json FROM journal
                         WHERE instance_id = ?1 AND seq > 0 ORDER BY seq ASC",
                    )
                    .expect("prepare");
                let mut rows = stmt.query([&instance_id]).expect("query");
                let mut n = 0;
                while let Some(row) = rows.next().expect("next") {
                    let payload: String = row.get(0).expect("payload");
                    let _: serde_json::Value = serde_json::from_str(&payload).expect("json");
                    n += 1;
                }
                Ok(n)
            })
            .await
            .expect("read all journal")
    }

    /// Pending-interaction count on the WRITER thread, where `list_interactions`
    /// used to queue before the reader pool. Evidence harness only.
    pub async fn pending_interactions_via_writer(store: &Store) -> usize {
        store
            .run_named("test.pending_via_writer", |conn| {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM interactions WHERE state = 'pending'",
                    [],
                    |row| row.get(0),
                )?;
                Ok(count as usize)
            })
            .await
            .expect("pending via writer")
    }

    /// One journal-tail read on the WRITER thread, where `read_journal_tail`
    /// used to queue. Evidence harness only.
    pub async fn journal_tail_via_writer(store: &Store, instance_id: &str, limit: i64) -> usize {
        let instance_id = instance_id.to_owned();
        store
            .run_named("test.tail_via_writer", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT payload_json FROM (
                        SELECT payload_json, seq FROM journal
                        WHERE instance_id = ?1 ORDER BY seq DESC LIMIT ?2
                     ) ORDER BY seq ASC",
                )?;
                let mut rows = stmt.query(rusqlite::params![instance_id, limit])?;
                let mut n = 0;
                while let Some(row) = rows.next()? {
                    let payload: String = row.get(0)?;
                    let _ = serde_json::from_str::<serde_json::Value>(&payload);
                    n += 1;
                }
                Ok(n)
            })
            .await
            .expect("tail via writer")
    }

    /// A fully-populated `CommandRecord` serialized exactly as the
    /// `/v1/instances/{id}/commands` route emits it (camelCase, internal ledger
    /// columns skipped, `settlement` present). The OpenAPI test compares this
    /// against the documented `CommandRecord` properties so a field cannot be
    /// added to the struct (or renamed) without the spec drifting.
    pub fn sample_command_record_json() -> Value {
        let record = CommandRecord {
            command_id: "cmd_sample".into(),
            instance_id: Some("ins_sample".into()),
            host_id: "hst_sample".into(),
            operation: "instance.send".into(),
            state: "settled".into(),
            resolution: "clear".into(),
            forwarded: true,
            settlement_outcome: Some("rejected".into()),
            settlement_reason: Some("node rejected the send".into()),
            settlement_http_status: None,
            settlement_http_body: None,
            settlement: Some(CommandSettlement {
                outcome: "rejected".into(),
                reason: Some("node rejected the send".into()),
            }),
            payload: json!({ "input": { "type": "prompt" } }),
            idempotency_key: Some("idem_sample".into()),
            created_at: "2026-09-18T00:00:00.000Z".into(),
            updated_at: "2026-09-18T00:00:01.000Z".into(),
        };
        serde_json::to_value(record).expect("command record serializes")
    }

    /// The wire values the protocol assigns to `SettlementOutcome` (§2.5),
    /// serialized from the enum itself. The journal projection accepts exactly
    /// the values this enum parses, so the OpenAPI test diffs the documented
    /// `settlement.outcome` enum against this set: a protocol outcome (such as
    /// `expired`) cannot be persisted by the projection while missing from the
    /// spec and generated client.
    pub fn settlement_outcome_wire_values() -> BTreeSet<String> {
        use remuda_protocol::SettlementOutcome;
        [
            SettlementOutcome::Completed,
            SettlementOutcome::Rejected,
            SettlementOutcome::Cancelled,
            SettlementOutcome::Expired,
        ]
        .into_iter()
        .map(|outcome| {
            serde_json::to_value(outcome)
                .expect("settlement outcome serializes")
                .as_str()
                .expect("wire value is a string")
                .to_owned()
        })
        .collect()
    }
}

/// A bound Hub that shuts down when dropped.
pub struct RunningHub {
    /// Actual listen address (port may be ephemeral).
    pub addr: SocketAddr,
    /// Bootstrap token devices and Nodes exchange on first enroll.
    pub bootstrap_token: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    store: Option<Store>,
    /// Live in-process state, for tests that register a synthetic Node.
    state: AppState,
}

impl RunningHub {
    /// Borrow this Hub's store (audit queries, support tooling, tests).
    #[must_use]
    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// Test helper: insert an online host row directly (no WS enroll).
    #[doc(hidden)]
    pub async fn test_insert_host(&self, host_id: &str) -> anyhow::Result<()> {
        if let Some(store) = self.store.as_ref() {
            let host_id = host_id.to_owned();
            store
                .run_named("test_insert_host", move |conn| {
                    let now = crate::config::now_rfc3339();
                    conn.execute(
                        "INSERT OR REPLACE INTO hosts
                            (id, label, token_hash, state, last_seen_at, node_version, cli_json,
                             capabilities_json, created_at, transport, labels_json, herdr_json,
                             resources_json, max_instances, hostname, token_prefix)
                         VALUES (?1, ?1, 'x', 'online', ?2, '0.1.0-test', '[]', '{}', ?2,
                                 'outbound-wss', '[]', NULL, NULL, 8, 'scripted-node', NULL)",
                        rusqlite::params![host_id, now],
                    )?;
                    Ok(())
                })
                .await?;
        }
        Ok(())
    }

    /// Test helper: mint an instance-scoped agent credential (operator-only
    /// routes must reject it with 403).
    #[doc(hidden)]
    pub async fn test_mint_agent_token(
        &self,
        device_name: &str,
        instance_id: &str,
    ) -> anyhow::Result<String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let token = config::random_token();
        let hash = auth::hash_secret(&token)?;
        let prefix = auth::token_prefix(&token)
            .ok_or_else(|| anyhow::anyhow!("generated device token is not indexable"))?
            .to_owned();
        store
            .insert_device_as(
                device_name.into(),
                hash,
                prefix,
                "agent".into(),
                Some(instance_id.into()),
            )
            .await?;
        Ok(token)
    }

    /// Test helper: replace a host's live Node transport with a synthetic one
    /// whose every RPC returns `reply` (the full JSON-RPC frame; `None` models
    /// an unwritable session → 422). Insert the host row first.
    #[doc(hidden)]
    pub async fn test_set_node_reply(&self, host_id: &str, reply: Option<serde_json::Value>) {
        use std::sync::Arc as StdArc;
        self.state
            .nodes
            .insert(
                host_id.to_owned(),
                StdArc::new(crate::transport::ScriptedTransport::new(reply)),
            )
            .await;
    }

    /// Test helper: mount any synthetic Node transport (e.g. one that records
    /// the frames the Hub sends). Insert the host row first.
    #[doc(hidden)]
    pub async fn test_set_node_transport(
        &self,
        host_id: &str,
        transport: std::sync::Arc<dyn crate::transport::NodeTransport>,
    ) {
        self.state.nodes.insert(host_id.to_owned(), transport).await;
    }

    /// Test helper: block every Node-registry lookup (`kind_of`, `call`,
    /// `insert`) until `release` is sent (or dropped). Returns only once the
    /// lock is actually held, so a command POST parked afterwards is
    /// deterministically in its pre-forward-attempt window.
    #[doc(hidden)]
    pub async fn test_hold_node_lookups(&self, release: tokio::sync::oneshot::Receiver<()>) {
        let (acquired, on_acquired) = tokio::sync::oneshot::channel();
        let nodes = self.state.nodes.clone();
        tokio::spawn(async move {
            nodes.test_hold_lock_until(release, acquired).await;
        });
        on_acquired.await.expect("node-lock holder started");
    }

    /// Test helper: mount a synthetic Node whose every RPC blocks until the
    /// returned [`CallGate`](crate::CallGate) is opened, then answers
    /// `Ok(None)` (frame never queued). Lets a test hold a forward attempt in
    /// its mark-intent → rollback window while observing command GETs.
    #[doc(hidden)]
    pub async fn test_set_node_gated(&self, host_id: &str) -> crate::transport::CallGate {
        use std::sync::Arc as StdArc;
        let gate = crate::transport::CallGate::new();
        self.state
            .nodes
            .insert(
                host_id.to_owned(),
                StdArc::new(crate::transport::GatedTransport::new(gate.clone())),
            )
            .await;
        gate
    }

    /// Test helper: drop the live Node session for `host_id` (host goes offline).
    #[doc(hidden)]
    pub async fn test_disconnect_node(&self, host_id: &str) {
        self.state.nodes.remove(host_id).await;
    }

    /// Test helper: run the restart reconcile pass against the live state, as
    /// `spawn` does on startup. Models a Hub restart re-entering reconcile
    /// without tearing down the process.
    #[doc(hidden)]
    pub async fn test_reconcile(&self) {
        crate::gatequeue::reconcile(&self.state).await;
    }

    /// Test helper: run the node.hello unregister reconciliation (c-dirpicker
    /// r7 item 1) — aborts every still-unsettled workspace.unregister command
    /// on `host_id`.
    #[doc(hidden)]
    pub async fn test_reconcile_unsettled_unregisters(&self, host_id: &str) {
        crate::workspaces::abort_unsettled_unregisters_on_reconnect(&self.state, host_id)
            .await
            .expect("reconcile unsettled unregisters");
    }

    /// Test helper (round 6 item 3): arm a one-shot park point in the REAL
    /// unregister DELETE handler (`unregister == true`; right after the
    /// occupancy query, before the prepare RPC) or the REAL POST /v1/tasks
    /// handler (`false`; after the per-workspace guard is acquired, before the
    /// binding is published). Returns `(reached, release)`: the handler
    /// notifies `reached` when it parks and continues once `release` is sent.
    #[doc(hidden)]
    pub fn test_arm_race_barrier(
        &self,
        unregister: bool,
        host_id: &str,
        workspace_id: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let phase = if unregister {
            crate::workspaces::RacePhase::Unregister
        } else {
            crate::workspaces::RacePhase::TaskBind
        };
        self.state.race_barriers.insert(
            phase,
            (host_id.to_owned(), workspace_id.to_owned()),
            crate::workspaces::BarrierSlot {
                reached: reached.clone(),
                release: release_rx,
            },
        );
        (reached, release_tx)
    }

    /// Mint a scoped device token against this Hub's store (D-018).
    ///
    /// In-process equivalent of `POST /v1/login`, for components composed into
    /// the same process as the Hub (`hub --with-dispatcher`, `remuda dev`).
    /// They get a real, revocable device token instead of holding the pairing
    /// access code, which under D-018 pairs devices and nothing else.
    pub async fn mint_device_token(&self, device_name: &str) -> anyhow::Result<String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let token = config::random_token();
        let hash = auth::hash_secret(&token)?;
        let prefix = auth::token_prefix(&token)
            .ok_or_else(|| anyhow::anyhow!("generated device token is not indexable"))?
            .to_owned();
        store
            .insert_device(device_name.to_owned(), hash, prefix)
            .await
            .map_err(|err| anyhow::anyhow!("mint device token: {err}"))?;
        Ok(token)
    }

    /// Test-only seam: age one worker's recorded `watch.lastActivityAt` so an
    /// integration test can reach the stall transition without waiting for the
    /// real quiet window. The HTTP API derives that clock from live timing and
    /// deliberately cannot set it.
    #[doc(hidden)]
    pub async fn age_worker_last_activity_for_tests(
        &self,
        worker_id: &str,
        rfc3339: &str,
    ) -> anyhow::Result<()> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let stamp = remuda_protocol::Timestamp::try_from(rfc3339.to_string())
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        store
            .mutate_worker(worker_id.to_string(), move |row| {
                if let Some(watch) = row.watch.as_mut() {
                    watch.last_activity_at = Some(stamp);
                }
                Ok(())
            })
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        Ok(())
    }

    /// Test-only seam: mark one Hub-side instance row as having exited, so a
    /// guard that distinguishes live from ended sessions can be exercised
    /// without driving a real close/journal sequence.
    #[doc(hidden)]
    pub async fn test_mark_instance_exited(&self, instance_id: &str) -> anyhow::Result<()> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let instance_id = instance_id.to_owned();
        store
            .run_named("test_mark_instance_exited", move |conn| {
                conn.execute(
                    "UPDATE instances SET lifecycle = 'exited', activity = 'idle', updated_at = ?1
                     WHERE id = ?2",
                    ["2026-10-05T00:00:00Z", instance_id.as_str()],
                )?;
                Ok(())
            })
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        Ok(())
    }

    /// Test-only seam: mark one Hub-side instance row as having failed with
    /// an error exit, so the occupancy guard's ended treatment of `failed`
    /// (and the Node's independent liveness) can be exercised.
    #[doc(hidden)]
    pub async fn test_mark_instance_failed(&self, instance_id: &str) -> anyhow::Result<()> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let instance_id = instance_id.to_owned();
        store
            .run_named("test_mark_instance_failed", move |conn| {
                conn.execute(
                    "UPDATE instances SET lifecycle = 'failed', activity = 'idle', \
                     last_error = 'test error exit', updated_at = ?1 WHERE id = ?2",
                    ["2026-10-05T00:00:00Z", instance_id.as_str()],
                )?;
                Ok(())
            })
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        Ok(())
    }

    /// Test-only seam: insert a running task bound to a workspace, so the
    /// unregister occupancy guard's task branch can be exercised without the
    /// full create/lease flow.
    #[doc(hidden)]
    pub async fn test_insert_bound_task(
        &self,
        task_id: &str,
        project_id: &str,
        host_id: &str,
        workspace_id: &str,
        state: &str,
    ) -> anyhow::Result<()> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let (task_id, project_id, host_id, workspace_id, state) = (
            task_id.to_owned(),
            project_id.to_owned(),
            host_id.to_owned(),
            workspace_id.to_owned(),
            state.to_owned(),
        );
        store
            .run_named("test_insert_bound_task", move |conn| {
                conn.execute(
                    "INSERT INTO tasks (id, project_id, title, state, doc_json, created_at, updated_at)
                     VALUES (?1, ?2, 'bound', ?3, ?4, '2026-10-05T00:00:00Z', '2026-10-05T00:00:00Z')",
                    rusqlite::params![
                        task_id,
                        project_id,
                        state,
                        serde_json::json!({
                            "workspaceBinding": {
                                "hostId": host_id,
                                "workspaceId": workspace_id,
                                "mode": "reuse"
                            }
                        })
                        .to_string()
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        Ok(())
    }

    /// Test helper: mark a directly-inserted bound task done and archived so
    /// the workspace occupancy query no longer counts it.
    #[doc(hidden)]
    pub async fn test_finish_bound_task(&self, task_id: &str) -> anyhow::Result<()> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let task_id = task_id.to_owned();
        store
            .run_named("test_finish_bound_task", move |conn| {
                conn.execute(
                    "UPDATE tasks SET state = 'done',
                        doc_json = json_set(doc_json, '$.archivedAt', '2026-10-08T00:00:00Z'),
                        updated_at = '2026-10-08T00:00:00Z'
                     WHERE id = ?1",
                    rusqlite::params![task_id],
                )?;
                Ok(())
            })
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        Ok(())
    }

    /// Mint a single-use Node enroll token against this Hub's store (D-018).
    ///
    /// In-process equivalent of `POST /v1/hosts/enroll-token`, used by
    /// `remuda dev` to enroll its own local Node without the device access code.
    pub async fn mint_enroll_token(&self, ttl_minutes: u64) -> anyhow::Result<String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let token = config::random_token();
        let hash = auth::hash_secret(&token)?;
        let expires = time::OffsetDateTime::now_utc()
            + time::Duration::minutes(i64::try_from(ttl_minutes.max(1)).unwrap_or(60));
        let expires_at = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            expires.year(),
            u8::from(expires.month()),
            expires.day(),
            expires.hour(),
            expires.minute(),
            expires.second(),
            expires.millisecond()
        );
        store
            .insert_enroll_token(
                hash,
                auth::token_prefix(&token).map(str::to_string),
                "dev-local".into(),
                expires_at,
            )
            .await
            .map_err(|err| anyhow::anyhow!("mint enroll token: {err}"))?;
        Ok(token)
    }

    /// Stop HTTP/WS accept and wait for the SQLite writer thread to close.
    pub async fn shutdown(mut self) {
        self.shutdown_within(Duration::from_secs(5)).await;
    }

    /// Stop accepting, wait up to `budget` for the HTTP server to drain, then
    /// flush the journal (WAL checkpoint) within the remaining time.
    ///
    /// Managed Hubs (`remuda hub --managed`) call this with a 3-second budget
    /// so SIGTERM and supervisor-exit shutdowns stay inside the desktop
    /// shell's teardown window. Connections still in flight when the budget
    /// expires are aborted; journal durability is given a separate floor.
    pub async fn shutdown_within(&mut self, budget: Duration) {
        let started = std::time::Instant::now();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(budget, &mut task).await.is_err()
        {
            tracing::warn!("hub server did not drain in {budget:?}; aborting");
            task.abort();
            let abort_budget = budget
                .saturating_sub(started.elapsed())
                .max(Duration::from_millis(250));
            let _ = tokio::time::timeout(abort_budget, &mut task).await;
        }
        if let Some(store) = self.store.take() {
            // The writer checkpoints the WAL on Stop (journal flush). Keep a
            // floor even when the drain spent the whole budget.
            let remaining = budget
                .saturating_sub(started.elapsed())
                .max(Duration::from_millis(500));
            if tokio::time::timeout(remaining, store.close())
                .await
                .is_err()
            {
                tracing::error!("hub journal did not flush before shutdown deadline");
            }
        }
    }
}

impl Drop for RunningHub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.store.take();
    }
}

/// Serve until Ctrl-C (composition-root entry).
pub async fn serve(config: HubConfig) -> anyhow::Result<()> {
    let running = spawn(config).await?;
    tracing::info!(addr = %running.addr, "remuda hub listening");
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// Bind and spawn the Axum server. Used by tests and `remuda hub`.
pub async fn spawn(config: HubConfig) -> anyhow::Result<RunningHub> {
    spawn_inner(config, None).await
}

/// Spawn with a fake Web Push transport (tests).
pub async fn spawn_with_push(
    config: HubConfig,
    transport: Arc<dyn remuda_push::Transport>,
) -> anyhow::Result<RunningHub> {
    spawn_inner(config, Some(transport)).await
}

async fn spawn_inner(
    mut config: HubConfig,
    transport: Option<Arc<dyn remuda_push::Transport>>,
) -> anyhow::Result<RunningHub> {
    supervise::mark_started();
    proxy::configure_public_origin(&mut config)?;
    std::fs::create_dir_all(&config.data_dir)?;
    let bootstrap_resolution = resolve_bootstrap(&mut config)?;
    let bootstrap_token = config.bootstrap_token.clone();
    let store = Store::open(&config.data_dir)?;
    store
        .mark_all_hosts_offline()
        .await
        .map_err(|err| anyhow::anyhow!("mark hosts offline: {err}"))?;
    let secrets = FileSecretStore::open(config.data_dir.join("secrets"))
        .map_err(|err| anyhow::anyhow!("provider secrets: {err}"))?;
    let push = match transport {
        Some(t) => Some(PushService::open_with(
            &config.data_dir,
            OpenOptions::test(t),
        )?),
        None => match PushService::open(&config.data_dir) {
            Ok(svc) => Some(svc),
            Err(err) => {
                tracing::warn!(error = %err, "web push disabled");
                None
            }
        },
    };
    let state = AppState {
        config: Arc::new(config.clone()),
        store: store.clone(),
        secrets: Arc::new(secrets),
        nodes: crate::transport::ConnectedNodes::default(),
        bus: Bus::with_capacity(config.follow_buffer_events),
        tty: crate::ws::TtyRelay::default(),
        api_relay: crate::api_relay::ApiRelay::new(),
        push,
        followers: Followers::default(),
        blocked: BlockedWatch::default(),
        auth_limits: rate_limit::AuthRateLimits::new(rate_limit::LimitParams {
            ip_burst: config.auth_ip_burst,
            ip_refill_per_sec: config.auth_ip_refill_per_sec,
            global_burst: config.auth_global_burst,
            global_refill_per_sec: config.auth_global_refill_per_sec,
        }),
        agent_approvals: agent_approvals::AgentApprovals::new()?,
        challenges: passkeys::ChallengeStore::default(),
        gate_ref_swept_at: Arc::new(std::sync::Mutex::new(None)),
        race_barriers: crate::workspaces::RaceBarriers::default(),
    };
    store.expire_lost_hosts(config.host_lost_grace_ms).await?;
    // A Hub restart must not inherit yesterday's unacknowledged creates: they
    // would keep holding placement slots with no Node that can ever settle them.
    expire_stale_requested(&state, config.requested_grace_ms).await;
    // Batch 6: jobs left running by a previous Hub process re-enter the queue.
    crate::gatequeue::reconcile(&state).await;
    crate::gatequeue::tick(&state).await;
    let reaper_state = state.clone();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let addr = listener.local_addr()?;
    persist_listen(&config.data_dir, addr)?;
    // Bind established: a no-source start may now adopt an explicit-marker'd
    // persisted token, or commit the in-memory mint of a token-less recovery
    // path. A failed bind above aborts before this, leaving the provenance
    // marker (and rotation refusal) intact.
    if bootstrap_resolution == BootstrapResolution::AdoptAfterBind {
        adopt_bootstrap_after_bind(&config.data_dir, &config.bootstrap_token)?;
    }
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let shutdown = async {
            let _ = rx.await;
        };
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .into_future();
        tokio::pin!(server);
        let mut interval = tokio::time::interval(Duration::from_millis(
            crate::gatequeue::SCHEDULE_TICK_MILLIS,
        ));
        loop {
            tokio::select! {
                result = &mut server => {
                    if let Err(err) = result { tracing::error!(error = %err, "hub server exited"); }
                    break;
                }
                _ = interval.tick() => {
                    if let Err(err) = reaper_state.store.expire_lost_hosts(config.host_lost_grace_ms).await {
                        tracing::error!(error = %err, "host-lost sweep failed");
                    }
                    expire_stale_requested(&reaper_state, config.requested_grace_ms).await;
                    crate::gatequeue::tick(&reaper_state).await;
                }
            }
        }
    });
    Ok(RunningHub {
        addr,
        bootstrap_token,
        shutdown: Some(tx),
        task: Some(task),
        store: Some(store),
        state,
    })
}

/// Fail `requested` instances the Node never acknowledged.
///
/// Each expiry gets a Hub-authored journal diagnostic so the row explains
/// itself, and stops counting toward the host's `maxInstances` ceiling.
async fn expire_stale_requested(state: &AppState, window_ms: u64) {
    let expired = match state.store.expire_stale_requested(window_ms).await {
        Ok(expired) => expired,
        Err(error) => {
            tracing::error!(%error, "stale-requested sweep failed");
            return;
        }
    };
    for (host_id, instance_id) in expired {
        tracing::warn!(
            %host_id,
            %instance_id,
            window_ms,
            "create was never acknowledged by the node; expiring to failed"
        );
        crate::ws::publish_hub_diagnostic(
            state,
            &instance_id,
            "create_never_acknowledged",
            "create was never acknowledged by the node; expired to failed",
        )
        .await;
    }
}

/// Axum router (HTTP + WS + static).
pub fn router(state: AppState) -> Router {
    let mut app = Router::new()
        .merge(http::routes())
        .merge(hosts::routes())
        .merge(ssh_hosts::routes(state.clone()))
        .merge(instances::routes())
        .merge(interactions::routes())
        .merge(tty::routes())
        .merge(ws::routes())
        .merge(placement::routes())
        .merge(projects::routes())
        .merge(tasks::routes())
        .merge(fleet::routes())
        .merge(devices::routes())
        .merge(passkeys::routes())
        .merge(providers::routes())
        .merge(supply::routes())
        .merge(agent_scope::routes())
        .merge(objects::routes(state.config.attachment_max_bytes))
        .merge(host_files::routes(state.config.attachment_max_bytes))
        .merge(host_dirs::routes())
        .merge(attachments::routes())
        .merge(workspaces::routes())
        .merge(workers::routes())
        .merge(gatequeue::routes())
        .merge(worker_watch::routes())
        .merge(bot::routes());
    if let Some(push) = state.push.clone() {
        app = app.nest_service("/push", push_http::nest(push, state.store.clone()));
    }
    app.fallback(static_fallback)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            agent_scope::restrict_agent_routes,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::limit_auth_attempts,
        ))
        .layer(axum::middleware::map_response(web::security_headers))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            proxy::secure_cookies,
        ))
        .with_state(state)
}

async fn static_fallback(State(state): State<AppState>, uri: Uri) -> Response {
    web::static_handler(uri, state.config.web_root.clone()).await
}
