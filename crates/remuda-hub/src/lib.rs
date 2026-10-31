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
mod digest;
mod error;
mod fence;
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
/// Test support for the D-057 commit-time initiator authority.
#[doc(hidden)]
pub mod agent_scope_test_support {
    pub use crate::agent_scope::CallerAuthority;
}
#[doc(hidden)]
pub mod usage_store_test_support {
    use rusqlite::Connection;

    pub use crate::store::JournalRecord;
    pub use crate::usage_store::{
        RollupRequest, aggregate_instance, aggregate_supply, insert_usage_event, migrate,
        project_usage_event, rollup_instance,
    };

    /// An in-memory, fully migrated Hub DB for projection/rollup tests that do
    /// not need a running Hub or the writer/reader split.
    pub fn migrated_memory() -> rusqlite::Result<Connection> {
        let conn = Connection::open_in_memory()?;
        crate::usage_store::migrate(&conn)?;
        Ok(conn)
    }
}
mod inventory;
mod maintenance;
mod memory;
mod model_catalog;
/// Built-in model catalog revision (tests compare the served catalog against it).
pub use model_catalog::CATALOG_REVISION;
mod node_ops;
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
mod seen;
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
/// Interactions invalidated when an instance generation ends — returned by the
/// terminal settle methods and broadcast on the follow bus (c-cardsettle).
pub use store::Settlement;
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
    /// c-cardsettle: DEDICATED low-traffic bus for settlement control notices.
    /// These ride separately from the high-volume journal bus so a follower
    /// lagging on a journal burst during page load cannot permanently miss the
    /// one-shot "your card was invalidated" notice (settlements are rare and
    /// tiny, so this channel effectively never lags).
    settlement_bus: tokio::sync::broadcast::Sender<crate::ws::SettlementNotice>,
    /// c-cardsettle r8 item 4: publication-order lock for settlements. Held
    /// across the settling transaction AND the bus publish, so notices from
    /// two concurrent settles (a journal exit and an HTTP delete, or two
    /// sweeps in the same millisecond) always enter the follower ring in the
    /// order they committed — a lagging follower's strictly-forward cursor
    /// can then never skip a lower-ordered row of an earlier transaction. The
    /// bus send is synchronous (it never awaits a reader), so the section
    /// only serialises two settling writers for the duration of the send.
    settlement_publish: Arc<tokio::sync::Mutex<()>>,
    /// c-cardsettle r11 item 1: test-only park seam for a follower's
    /// settlement pump; always present but never armed in production.
    #[cfg(any(test, feature = "test-faults"))]
    follow_parks: crate::ws::FollowParks,
    /// c-cardsettle-fu r1 item 2: test-only park seam for the detached
    /// irreversible delete operation; never armed in production.
    #[cfg(any(test, feature = "test-faults"))]
    operation_parks: crate::ws::OperationParks,
    /// c-cardsettle-fu2 r4 item 3: test-only per-interaction count of
    /// settlement notices actually put on the bus. Lets a test prove a
    /// parked delete has NOT broadcast without reading the follower socket
    /// during the park window (cancelled WS reads starve the next poll).
    #[cfg(any(test, feature = "test-faults"))]
    test_settlement_counts: Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
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
    /// r27 item 2: per-job guards serializing legacy gate `/resolve`
    /// validation/abort against verdict-applied evidence-deferral transitions.
    pub(crate) gate_decision_guards: crate::gatequeue::GateDecisionGuards,
    /// D-057 §7.4: the in-memory fence map the send gate reads, and per-host
    /// fence delivery state.
    pub(crate) fences: crate::fence::FenceState,
}

impl AppState {
    /// c-cardsettle: publish the interactions a terminal write invalidated so
    /// an open inbox/session following one of those instances drops the card
    /// immediately instead of waiting for its next poll.
    ///
    /// These are Hub-side notices, NOT forged journal entries: each rides the
    /// follow bus as a seq-less `settlement` control frame
    /// ([`crate::ws::FollowEvent::settlement`]), so it can never be inserted
    /// into a journal or make a follower mark its journal stale. A fresh
    /// follower converges against the durable (already-invalidated) rows
    /// regardless, so a missed notice is self-healing.
    ///
    /// r9 item 3: notices are sent in the vector's order, which the store
    /// produces as ascending `settlement_events.seq` (AUTOINCREMENT assigned
    /// inside the settling transaction; one sweep's rows are selected by id).
    /// The follower's delivery max-seq cursor and its strict-forward lag
    /// recovery both rely on that monotonic publication order — never reorder
    /// or sort these before sending.
    /// Acquire the settlement publication-order lock (r8 item 4) for a
    /// settling transaction whose publication happens inline (the journal
    /// append batch), rather than through [`Self::publish_settlement`].
    pub(crate) async fn settlement_publish_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.settlement_publish.lock().await
    }

    /// Run a settlement-producing store operation and publish its settlement
    /// under the per-Hub publication-order lock (c-cardsettle r8 item 4). The
    /// guard is acquired BEFORE the commit and released after the bus send, so
    /// notices can never overtake an earlier settlement's publication even
    /// when two settles race from different tasks (a journal exit vs an HTTP
    /// delete; same-millisecond sweeps).
    pub(crate) async fn publish_settlement<Fut, T>(
        &self,
        op: Fut,
    ) -> Result<T, crate::store::StoreError>
    where
        Fut: std::future::Future<
                Output = Result<(T, crate::store::Settlement), crate::store::StoreError>,
            >,
    {
        let _guard = self.settlement_publish.lock().await;
        let (value, settlement) = op.await?;
        self.broadcast_settlement(&settlement);
        Ok(value)
    }

    /// Same publication ordering for an operation that produces only a
    /// [`Settlement`].
    pub(crate) async fn publish_settlement_unit<Fut>(
        &self,
        op: Fut,
    ) -> Result<(), crate::store::StoreError>
    where
        Fut: std::future::Future<Output = Result<crate::store::Settlement, crate::store::StoreError>>,
    {
        let _guard = self.settlement_publish.lock().await;
        let settlement = op.await?;
        self.broadcast_settlement(&settlement);
        Ok(())
    }

    pub(crate) fn broadcast_settlement(&self, settlement: &Settlement) {
        for settled in &settlement.interactions {
            // Publish on the DEDICATED settlement bus (not the journal bus): a
            // follower lagging on a journal burst during page load must not
            // miss the one-shot terminal notice. Recent settlements are also
            // replayed to unfiltered followers on connect (see follow_session).
            let _ = self.settlement_bus.send(crate::ws::SettlementNotice {
                instance_id: settled.instance_id.clone(),
                interaction_id: settled.interaction_id.clone(),
                state: "invalidated".to_string(),
                // r10 item 4(a): carry the durable reason verbatim. The frame
                // used to hard-code generation-ended, so a demotion
                // invalidation (reason agent-demoted) reached followers with
                // the wrong end reason.
                reason: settled.reason.clone(),
                seq: settled.seq,
            });
            // r4 item 3: record a broadcast the bus actually accepted; keyed
            // by interaction id so unrelated background settles cannot move
            // a test's counter.
            self.note_test_settlement_broadcast(&settled.interaction_id);
        }
    }

    /// r4 item 3 test seam: count a broadcast per interaction id.
    #[cfg(any(test, feature = "test-faults"))]
    fn note_test_settlement_broadcast(&self, interaction_id: &str) {
        *self
            .test_settlement_counts
            .lock()
            .unwrap()
            .entry(interaction_id.to_owned())
            .or_insert(0) += 1;
    }

    #[cfg(not(any(test, feature = "test-faults")))]
    fn note_test_settlement_broadcast(&self, _interaction_id: &str) {}

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

    pub use crate::store::{
        APPEND_CHUNK_MAX, JOURNAL_WINDOW_BYTES, JOURNAL_WINDOW_ROWS, SCHEMA_USER_VERSION, Store,
    };

    /// D-057 §7.3 commit-time check error (the initiator suite matches on it).
    pub use crate::store::StoreError;

    /// Test-only gate-cancel writer-race seam (cancel handler pre-read vs.
    /// the state observed inside the writer job).
    #[cfg(any(test, feature = "test-faults"))]
    pub use crate::gatequeue::TestGateCancelRace;

    /// Test-only seam: which interloper settles a Canceling gate job inside
    /// the next `mutate_gate_job` writer (duplicate verdict / grace expiry).
    #[cfg(any(test, feature = "test-faults"))]
    pub use crate::gatequeue::TestGateFinishBeforeWrite;

    /// Test-only apply_result boundary barrier.
    #[cfg(any(test, feature = "test-faults"))]
    pub use crate::gatequeue::GateApplyBarrier;

    /// r18 item 7 test-only: post-evidence-persist barrier.
    #[cfg(any(test, feature = "test-faults"))]
    pub use crate::gatequeue::SimpleBarrier;

    /// Durable usage-event row shape for tests that seed usage through the
    /// writer (see [`seed_usage_events`]).
    pub use crate::usage_store::UsageEventRow;

    /// Test-only fault guard: arm failures on ONE store (per-Store flags,
    /// `test-faults` feature only) and clear them on drop, so a failed
    /// assertion can never leave a fault armed and other Stores are never
    /// affected.
    #[cfg(any(test, feature = "test-faults"))]
    pub fn arm_lease_lookup_failure(store: &Store) -> impl Drop {
        crate::store::test_faults::arm_lease_lookup_failure(store.fault_flags());
        struct LeaseLookupFault {
            flags: std::sync::Arc<crate::store::test_faults::FaultFlags>,
        }
        let flags = store.fault_flags().clone();
        impl Drop for LeaseLookupFault {
            fn drop(&mut self) {
                crate::store::test_faults::clear(&self.flags);
            }
        }
        LeaseLookupFault { flags }
    }

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

    /// A seat's delegation: no grants, a restart policy, so the insert
    /// creates a continuity lineage row with itself as the current chapter.
    pub fn continuity_delegation() -> InstanceDelegation {
        InstanceDelegation {
            restart: Some(crate::store::RestartPolicy {
                on_process_loss: true,
                max_per_hour: 3,
            }),
            ..InstanceDelegation::default()
        }
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

    /// Test fixture: insert durable `usage_events` directly on the writer
    /// connection (production projects them via `usage_store::observe_journal`
    /// from `kind:"usage"` journal records). Returns how many were inserted.
    pub async fn seed_usage_events(
        store: &Store,
        rows: Vec<crate::usage_store::UsageEventRow>,
    ) -> anyhow::Result<usize> {
        store
            .run_named("test.seed_usage_events", move |conn| {
                let mut inserted = 0usize;
                for row in &rows {
                    if crate::usage_store::insert_usage_event(conn, row)? {
                        inserted += 1;
                    }
                }
                Ok::<_, crate::store::StoreError>(inserted)
            })
            .await
            .map_err(Into::into)
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
            // D-057 §7.1: Agent-initiated commands serialize the stamped
            // initiator (the device id never leaves the Hub).
            initiator: Some(remuda_protocol::Initiator {
                instance_id: "ins_sample".into(),
                lineage_id: "ins_sample".into(),
                generation: 1,
            }),
            initiator_device_id: None,
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
            SettlementOutcome::Withheld,
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

/// Test-only `'static` handle to a running Hub's state.
#[doc(hidden)]
#[derive(Clone)]
pub struct HubTestHandle {
    state: AppState,
}

impl HubTestHandle {
    /// Mount a synthetic Node transport (see
    /// [`RunningHub::test_set_node_transport`]).
    pub async fn test_set_node_transport(
        &self,
        host_id: &str,
        transport: std::sync::Arc<dyn crate::transport::NodeTransport>,
    ) {
        self.state.nodes.insert(host_id.to_owned(), transport).await;
    }

    /// Arm the forward_if_online pre-send barrier.
    /// D-057 §7.10 seams: park a direct op between admission and send
    /// intent (`node-op-admitted`, key op id) or a gate dispatch between its
    /// claim and the send gate (`gate-claimed`, key job id). Returns
    /// (reached, release).
    pub fn test_arm_fence_seam(
        &self,
        phase: &str,
        host_id: &str,
        key: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let phase = match phase {
            "node-op-admitted" => crate::workspaces::RacePhase::NodeOpAdmitted,
            "gate-claimed" => crate::workspaces::RacePhase::GateClaimed,
            "forward-intent-marked" => crate::workspaces::RacePhase::ForwardIntentMarked,
            "forward-send" => crate::workspaces::RacePhase::ForwardSend,
            other => panic!("unknown fence seam {other}"),
        };
        let reached: std::sync::Arc<tokio::sync::Notify> =
            std::sync::Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.state.race_barriers.insert(
            phase,
            (host_id.to_owned(), key.to_owned()),
            crate::workspaces::BarrierSlot {
                reached: reached.clone(),
                release: release_rx,
            },
        );
        (reached, release_tx)
    }

    /// D-057 §7.10 send-gate seam: park `forward_if_online` for `command_id`
    /// right after its forward intent committed and before the send gate
    /// reads the fence map. Returns (reached, release).
    pub fn test_arm_forward_intent_barrier(
        &self,
        host_id: &str,
        command_id: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let reached: std::sync::Arc<tokio::sync::Notify> =
            std::sync::Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.state.race_barriers.insert(
            crate::workspaces::RacePhase::ForwardIntentMarked,
            (host_id.to_owned(), command_id.to_owned()),
            crate::workspaces::BarrierSlot {
                reached: reached.clone(),
                release: release_rx,
            },
        );
        (reached, release_tx)
    }

    pub fn test_arm_forward_send_barrier(
        &self,
        host_id: &str,
        command_id: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let reached: std::sync::Arc<tokio::sync::Notify> =
            std::sync::Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.state.race_barriers.insert(
            crate::workspaces::RacePhase::ForwardSend,
            (host_id.to_owned(), command_id.to_owned()),
            crate::workspaces::BarrierSlot {
                reached: reached.clone(),
                release: release_rx,
            },
        );
        (reached, release_tx)
    }

    /// D-057 §7.8 test seam: a restart decision's F with successor, as the C1
    /// supervisor (ma-restart) will run it — generation read, then the
    /// `restart-read` continuation hook, then the CAS'd transaction. Returns
    /// the successor id, or `None` when the CAS lost (another F won).
    pub async fn test_restart_lineage(&self, lineage_id: &str) -> anyhow::Result<Option<String>> {
        let lineage = self
            .state
            .store
            .get_lineage(lineage_id.to_owned())
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown lineage {lineage_id}"))?;
        crate::store::run_continuation_hook("restart-read", lineage_id);
        let current = self
            .state
            .store
            .get_instance(lineage.current_instance_id.clone())
            .await?
            .ok_or_else(|| anyhow::anyhow!("current chapter missing"))?;
        let result = self
            .state
            .store
            .continuation_resume(crate::store::ContinuationResumeRequest {
                addressed_instance_id: current.instance_id.clone(),
                expected_generation: lineage.generation,
                host_id: current.host_id.clone(),
                spec: current.spec_for_resume(),
                operation: "instance.create".into(),
                prompt: None,
                origin: "human".into(),
                title: current.title.clone(),
                driver: current.driver.clone(),
                actor: serde_json::json!({"by": "hub-supervisor"}),
            })
            .await?;
        match result {
            crate::store::ContinuationResumeResult::Resumed(resumed) => {
                self.state.broadcast_settlement(&resumed.fence.settlement);
                crate::fence::after_commit(&self.state, &resumed.fence, None).await;
                Ok(Some(resumed.successor.instance_id))
            }
            crate::store::ContinuationResumeResult::Superseded { .. } => Ok(None),
        }
    }

    /// One scheduler tick (claims + dispatches), on an owned handle.
    pub async fn test_gate_tick(&self) {
        crate::gatequeue::tick(&self.state).await;
    }

    /// [`RunningHub::test_call_node_op`] on an owned handle (spawnable).
    pub async fn test_call_node_op(
        &self,
        host_id: &str,
        method: &'static str,
        op_id: String,
        initiator: Option<remuda_protocol::Initiator>,
        device_id: Option<String>,
    ) -> anyhow::Result<bool> {
        let frame = crate::node_ops::call_admitted_frame(
            &self.state,
            host_id,
            method,
            None,
            op_id,
            serde_json::json!({}),
            std::time::Duration::from_secs(5),
            &crate::node_ops::NodeOpAuth {
                initiator,
                device_id,
            },
        )
        .await?;
        Ok(frame.is_some())
    }

    /// Run one queued command through the real `forward_if_online` send path.
    pub async fn test_forward_command_online(
        &self,
        command_id: &str,
    ) -> anyhow::Result<crate::store::CommandRecord> {
        let command = self
            .state
            .store
            .get_command(command_id.to_owned())
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown command {command_id}"))?;
        Ok(crate::http::forward_if_online(&self.state, command, true).await?)
    }
}

impl RunningHub {
    /// Borrow this Hub's store (audit queries, support tooling, tests).
    #[must_use]
    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// Test helper: the live AppState (drives production handlers directly).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    #[must_use]
    pub fn test_state(&self) -> &AppState {
        &self.state
    }

    /// Test helper: apply a gate verdict straight through the production
    /// `apply_result` handler on a borrowed AppState (for 'static spawn tasks).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_apply_gate_result_static(
        state: &AppState,
        job_id: &str,
        result: serde_json::Value,
    ) {
        let result: remuda_protocol::GateRunResult =
            serde_json::from_value(result).expect("valid GateRunResult");
        // Same session resolution as test_apply_gate_result: present the
        // verdict on the session the live dispatch rode.
        let session = state
            .store
            .gate_job_dispatch_session(job_id)
            .await
            .ok()
            .flatten()
            .and_then(|v| u64::try_from(v).ok());
        crate::gatequeue::apply_result(state, job_id, result, session).await;
    }

    /// Test helper: the boot epoch this Hub process allocated.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub fn boot_epoch_value(&self) -> u64 {
        self.state.nodes.boot_epoch()
    }

    /// Test helper: an owned (`'static` + `Clone`) handle to the Hub state so
    /// tests can drive a handler from a spawned task while orchestrating link
    /// swaps on the main task.
    #[doc(hidden)]
    #[must_use]
    pub fn test_handle(&self) -> HubTestHandle {
        HubTestHandle {
            state: self.state.clone(),
        }
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
                         VALUES (?1, ?1, 'x', 'online', ?2, '0.1.0-test', '[]',
                                 '{\"lineageFence\":true}', ?2,
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

    /// Test helper (c-dirpicker r11 item 2): mount a synthetic transport AS a
    /// specific durable link generation — exactly what the `node.hello` arm
    /// does after `bump_host_link_generation`. Lets interleave tests model the
    /// bump→insert window with the previous session still registered.
    #[doc(hidden)]
    pub async fn test_set_node_transport_with_generation(
        &self,
        host_id: &str,
        transport: std::sync::Arc<dyn crate::transport::NodeTransport>,
        link_generation: i64,
    ) {
        self.state
            .nodes
            .insert_with_generation(host_id.to_owned(), transport, link_generation)
            .await;
    }

    /// Test helper (r41 item 1): park the next hello for `host_id` just before
    /// it installs its registry slot. Returns (parked, release).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_arm_hello_install_park(
        &self,
        host_id: &str,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.state.nodes.arm_install_park(host_id).await
    }

    /// Test helper (r41 item 2): park the next socket teardown for `host_id`
    /// before its gate-session recovery step. Returns (parked, release, done).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_arm_teardown_recovery_park(
        &self,
        host_id: &str,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        self.state.nodes.arm_teardown_recovery_park(host_id).await
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

    /// Test helper: the host's CURRENT authenticated carrier session, if any.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_node_session(&self, host_id: &str) -> Option<u64> {
        self.state.nodes.generation_of(host_id).await
    }

    /// Test helper: set a Landing row's `landing_at` timestamp directly (RFC
    /// 3339), so recovery-window expiry can be exercised without waiting.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_set_landing_at(&self, job_id: &str, at: &str) -> anyhow::Result<()> {
        let at = at.to_owned();
        let job_id = job_id.to_owned();
        let store = self
            .store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        store
            .run_named("test_set_landing_at", move |conn| {
                conn.execute(
                    "UPDATE gate_jobs SET landing_at = ?2 WHERE id = ?1",
                    rusqlite::params![job_id, at],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Test helper: park a Landing row in the outcome-unknown state exactly
    /// like the expired-recovery path (flag + reason), without waiting out the
    /// real recovery window.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_mark_landing_unknown(
        &self,
        job_id: &str,
        note: &str,
    ) -> anyhow::Result<serde_json::Value> {
        use remuda_protocol::GateJobState;
        let note = note.to_owned();
        let store = self
            .store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let updated = store
            .mutate_gate_job(job_id, move |row, _conn| {
                if row.state != GateJobState::Landing {
                    return None;
                }
                if !row.landing_outcome_unknown {
                    row.landing_outcome_unknown = true;
                    row.reason = Some(note.clone());
                }
                Some(())
            })
            .await?
            .ok_or_else(|| anyhow::anyhow!("job is not Landing"))?;
        Ok(serde_json::to_value(updated).expect("gate job serializes"))
    }

    /// Test helper: this Hub process's boot epoch.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub fn test_boot_epoch(&self) -> u64 {
        self.state.nodes.boot_epoch()
    }

    /// Test helper: resolve once a carrier hello replacement for `host_id` is
    /// BLOCKED on the per-host authority guard (an in-flight Landing verdict
    /// writer is holding it). Deterministic witness for the r15 item 3
    /// takeover/writer serialization.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_wait_node_authority_contended(&self, host_id: &str) {
        let notify = self.state.nodes.authority_write_notify(host_id).await;
        notify.notified().await;
    }

    /// Test helper: run the restart reconcile pass against the live state, as
    /// `spawn` does on startup. Models a Hub restart re-entering reconcile
    /// without tearing down the process.
    #[doc(hidden)]
    pub async fn test_reconcile(&self) {
        crate::gatequeue::reconcile(&self.state).await;
        // r13 item 1: Landing rows are no longer settled at startup; they are
        // probed against the remote once a carrier is reachable. The fake lane
        // in gate-queue tests stays connected, so run the same bounded pass the
        // post-hello task runs (a row never probed has a NULL throttle stamp).
        crate::gatequeue::reconcile_landings(&self.state, None).await;
    }

    /// Test-only: run a FORCED Landing reconciliation for one lane host,
    /// bypassing the probe throttle — deterministic stand-in for the
    /// post-hello forced pass (r16 flake fix).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_force_landing_reconcile(&self, host_id: &str) {
        crate::gatequeue::reconcile_landings(&self.state, Some(host_id)).await;
    }

    /// Test-only: claim a queued gate job the way the scheduler tick would,
    /// without needing a registered lane Node. Returns the claimed job.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_claim_gate_job(
        &self,
        job_id: &str,
        lane_id: &str,
        host_id: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let host: remuda_protocol::HostId = host_id
            .parse()
            .map_err(|err: remuda_protocol::WireValueError| anyhow::anyhow!("{err}"))?;
        let claim = self
            .store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?
            .claim_gate_job(job_id, lane_id.to_owned(), host)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(match claim {
            crate::gatequeue::GateJobClaim::Claimed(job) => {
                Some(serde_json::to_value(job).expect("gate job serializes"))
            }
            crate::gatequeue::GateJobClaim::FencedCanceled(_)
            | crate::gatequeue::GateJobClaim::NotReady => None,
        })
    }

    /// Test-only: run one scheduler tick deterministically (no wait for the
    /// 1s interval).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_gate_tick(&self) {
        crate::gatequeue::tick(&self.state).await;
    }

    /// Test-only: apply a gate run verdict through the real `apply_result`
    /// path (pre-read + writer job), for the cancel-vs-result race suite.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_apply_gate_result(
        &self,
        job_id: &str,
        result: serde_json::Value,
    ) -> anyhow::Result<()> {
        let result: remuda_protocol::GateRunResult =
            serde_json::from_value(result).map_err(|err| anyhow::anyhow!("{err}"))?;
        // Represent the verdict as arriving on the session the row's live
        // dispatch rode (the production RPC-callback identity). Tests that need
        // a replacement carrier drive the raw gate.event/reply path instead.
        let session = self
            .state
            .store
            .gate_job_dispatch_session(job_id)
            .await
            .ok()
            .flatten()
            .and_then(|v| u64::try_from(v).ok());
        crate::gatequeue::apply_result(&self.state, job_id, result, session).await;
        Ok(())
    }

    /// Test-only: the current stored gate job doc.
    #[doc(hidden)]
    pub async fn test_get_gate_job(
        &self,
        job_id: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let job = self
            .store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?
            .get_gate_job(job_id)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(job.map(|job| serde_json::to_value(job).expect("gate job serializes")))
    }

    /// Test-only: the Hub-only carrier session a gate job is currently
    /// dispatched/granted to (r16 item 6).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_gate_dispatch_session(&self, job_id: &str) -> anyhow::Result<Option<i64>> {
        Ok(self
            .store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?
            .gate_job_dispatch_session(job_id)
            .await?)
    }

    /// Test-only: bind a gate job's dispatch/grant session to an arbitrary
    /// value (r16 item 7 — makes a snapshot settle writer refuse while the
    /// remote probe still establishes landed).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_set_gate_dispatch_session_raw(
        &self,
        job_id: &str,
        session: u64,
    ) -> anyhow::Result<()> {
        self.store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?
            .set_gate_dispatch_session(job_id, session)
            .await?;
        Ok(())
    }

    /// Test-only: the ordered audit-log actions recorded for one gate job id
    /// (e.g. `gate.running`, `gate.landed`).
    #[doc(hidden)]
    pub async fn test_gate_audit_actions(&self, job_id: &str) -> anyhow::Result<Vec<String>> {
        let rows = self
            .store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?
            .audit_for(job_id.to_owned())
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                row.get("action")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .collect())
    }

    /// Test-only: run the production generation-gated evidence transaction
    /// (`Store::insert_gate_log`) and return its object-id result (None when
    /// the verdict generation no longer matches the row's attempt). Drives the
    /// real writer, not a test copy (r12 item 8).
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub async fn test_insert_gate_log(
        &self,
        project_id: &str,
        job_id: &str,
        bytes: Vec<u8>,
        digest: String,
        created_by: &str,
        generation: u32,
        session: Option<u64>,
        host: Option<&str>,
        merge_sha: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        self.store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?
            .insert_gate_log(
                project_id,
                job_id,
                bytes,
                digest,
                created_by,
                crate::gatequeue::LogAuth {
                    generation,
                    session,
                    merge_sha: merge_sha.map(str::to_owned),
                },
                host,
            )
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    /// Test-only: drive the production landing-probe writer with the exact
    /// (attempts, merge, session) tuple the probe was taken for. Returns the
    /// settled job JSON only when the live row still names that attempt (r14 R2).
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_settle_landing_probe(
        &self,
        job_id: &str,
        expected_attempts: u32,
        expected_merge: String,
        expected_session: Option<i64>,
        expected_host: &str,
        landed: bool,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let store = self
            .store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let mut snapshot = store
            .get_gate_job(job_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("job missing"))?;
        snapshot.attempts = expected_attempts;
        snapshot.merge_sha = Some(expected_merge);
        let settled = store
            .settle_landing_probe(&snapshot, expected_session, expected_host, landed)
            .await?;
        Ok(settled.map(|job| serde_json::to_value(job).expect("gate job serializes")))
    }

    /// Test-only: take the Running → Landing grant for the live claimed attempt.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_gate_land_grant(
        &self,
        job_id: &str,
        host_id: &str,
        merge_sha: &str,
        session: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let store = self
            .store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let job = store
            .get_gate_job(job_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("job missing"))?;
        let host: remuda_protocol::HostId = host_id
            .parse()
            .map_err(|err: remuda_protocol::WireValueError| anyhow::anyhow!("{err}"))?;
        // The production dispatcher stamps the bound socket session before the
        // grant frame is sent; mirror that so the grant tuple is complete.
        let session_i64 = i64::try_from(session)?;
        store
            .set_gate_dispatch_session(job_id, session)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        let answer = store
            .gate_land_grant(
                job_id,
                host_id,
                job.attempts,
                "test-epoch",
                merge_sha,
                session,
            )
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        let _ = host;
        let _ = session_i64;
        Ok(serde_json::json!({
            "granted": answer.granted,
            "landingInProgress": answer.landing_in_progress,
            "state": answer.state,
        }))
    }

    /// r18 item 8 test-only: call the STORE grant directly, bypassing the
    /// RunningHub wrapper's u64→i64 prevalidation, to prove the STORE itself
    /// refuses a non-positive-i64 session (epoch ≥ 2^31). Reverting the store's
    /// `try_from` guard makes this call persist a 0 session and succeed.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-faults"))]
    pub async fn test_store_gate_land_grant_raw(
        &self,
        job_id: &str,
        host_id: &str,
        merge_sha: &str,
        session: u64,
    ) -> anyhow::Result<()> {
        let store = self
            .store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?;
        let attempts = store
            .get_gate_job(job_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("job missing"))?
            .attempts;
        store
            .gate_land_grant(job_id, host_id, attempts, "test-epoch", merge_sha, session)
            .await
            .map(|_| ())
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    /// Test-only: run the §7.5 direct-op admission (check + `admitted` row)
    /// without a live Node, for worker.provision/remove coverage while the
    /// worker routes themselves stay Agent-403 (ma-admission opens them).
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub async fn test_admit_node_op(
        &self,
        op_id: String,
        host_id: String,
        method: &'static str,
        subject: Option<String>,
        initiator: Option<remuda_protocol::Initiator>,
        device_id: Option<String>,
    ) -> anyhow::Result<()> {
        self.store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?
            .admit_node_op(
                op_id,
                host_id,
                method,
                subject,
                &crate::node_ops::NodeOpAuth {
                    initiator,
                    device_id,
                },
            )
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    /// Test-only: drive one admitted direct RPC through the full §7.5
    /// boundary (admit → send-intent → call → settle). Returns `true` when a
    /// frame was written, `false` when the host was not connected.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub async fn test_call_node_op(
        &self,
        host_id: &str,
        method: &'static str,
        op_id: String,
        initiator: Option<remuda_protocol::Initiator>,
        device_id: Option<String>,
    ) -> anyhow::Result<bool> {
        let frame = crate::node_ops::call_admitted_frame(
            &self.state,
            host_id,
            method,
            None,
            op_id.clone(),
            serde_json::json!({}),
            std::time::Duration::from_secs(5),
            &crate::node_ops::NodeOpAuth {
                initiator,
                device_id,
            },
        )
        .await?;
        Ok(frame.is_some())
    }

    /// Test-only: read a node_ops row's (state, outcome_json) pair.
    #[doc(hidden)]
    pub async fn test_node_op_state(
        &self,
        op_id: &str,
        host_id: &str,
    ) -> anyhow::Result<Option<(String, Option<String>)>> {
        self.store()
            .ok_or_else(|| anyhow::anyhow!("hub store already closed"))?
            .test_get_node_op(op_id.to_owned(), host_id.to_owned())
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    /// Test helper (c-cardsettle r8 item 3 / r9 item 3): backdate settlement
    /// history to a fixed RFC3339 timestamp — both the interactions'
    /// `updated_at` and their `settlement_events.created_at` log entries — so
    /// an integration test can create settlement history outside the
    /// 5-minute connect-replay window and prove the follower lag drain seeds
    /// at the durable max seq instead of walking the whole history.
    #[doc(hidden)]
    /// Backdate `interaction_ids` to `rfc3339` in BOTH tables and return the
    /// number of rows changed in EACH table, in order
    /// `(interactions, settlement_events)`. c-cardsettle-fu r1 item 4: callers
    /// assert the counts so a typo'd id / silent no-op UPDATE fails the test
    /// instead of leaving the backdate unobserved.
    #[doc(hidden)]
    pub async fn test_backdate_interactions(
        &self,
        interaction_ids: Vec<String>,
        rfc3339: &str,
    ) -> anyhow::Result<(usize, usize)> {
        let Some(store) = self.store.as_ref() else {
            anyhow::bail!("hub store already closed");
        };
        let ts = rfc3339.to_string();
        store
            .run_named("test_backdate_interactions", move |conn| {
                let mut interactions = 0usize;
                let mut settlement_events = 0usize;
                for id in &interaction_ids {
                    interactions += conn.execute(
                        "UPDATE interactions SET updated_at = ?1 WHERE id = ?2",
                        rusqlite::params![ts, id],
                    )?;
                    settlement_events += conn.execute(
                        "UPDATE settlement_events SET created_at = ?1 WHERE interaction_id = ?2",
                        rusqlite::params![ts, id],
                    )?;
                }
                Ok((interactions, settlement_events))
            })
            .await
            .map_err(Into::into)
    }

    /// Test helper: run the node.hello unregister reconciliation (c-dirpicker
    /// r7 item 1) — aborts every still-unsettled workspace.unregister command
    /// on `host_id` stamped by a link older than the host's current link
    /// generation (r10 item 1; a test modeling a reconnect bumps the
    /// generation first, exactly as an adopting node.hello does).
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

    /// Test helper (c-dirpicker r11 item 2): arm the park point between the
    /// forward-intent stamp and the prepare send of the REAL unregister DELETE
    /// handler — after the handler pinned its transport. A test parks the
    /// DELETE there, runs the next hello (generation bump + new transport
    /// insert), releases, and asserts the prepare/commit stayed on the pinned
    /// link.
    #[doc(hidden)]
    pub fn test_arm_forward_marked_barrier(
        &self,
        host_id: &str,
        workspace_id: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.state.race_barriers.insert(
            crate::workspaces::RacePhase::ForwardMarked,
            (host_id.to_owned(), workspace_id.to_owned()),
            crate::workspaces::BarrierSlot {
                reached: reached.clone(),
                release: release_rx,
            },
        );
        (reached, release_tx)
    }

    /// Test helper (c-dirpicker r12 item 1): arm the park point between the
    /// command INSERT and `mark_forward_intent` in the REAL unregister DELETE
    /// handler — after the handler pinned the (L1, generation) pair. A test
    /// parks the DELETE there, bumps the durable host generation (as the next
    /// hello would), and asserts the post-hello sweep still selects the row
    /// stamped with the pinned generation.
    #[doc(hidden)]
    pub fn test_arm_forward_queued_barrier(
        &self,
        host_id: &str,
        workspace_id: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let reached: std::sync::Arc<tokio::sync::Notify> =
            std::sync::Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.state.race_barriers.insert(
            crate::workspaces::RacePhase::ForwardQueued,
            (host_id.to_owned(), workspace_id.to_owned()),
            crate::workspaces::BarrierSlot {
                reached: reached.clone(),
                release: release_rx,
            },
        );
        (reached, release_tx)
    }

    /// Test helper (c-dirpicker r12 item 1): arm the park point between the
    /// transport pin and the command INSERT in the REAL unregister DELETE
    /// handler.
    #[doc(hidden)]
    pub fn test_arm_forward_pinned_barrier(
        &self,
        host_id: &str,
        workspace_id: &str,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let reached: std::sync::Arc<tokio::sync::Notify> =
            std::sync::Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.state.race_barriers.insert(
            crate::workspaces::RacePhase::ForwardPinned,
            (host_id.to_owned(), workspace_id.to_owned()),
            crate::workspaces::BarrierSlot {
                reached: reached.clone(),
                release: release_rx,
            },
        );
        (reached, release_tx)
    }

    /// c-cardsettle r11 item 1: arm a park on the REAL `/v1/follow`
    /// settlement pump, after it processes the live notice with seq
    /// `after_seq`. `reached` resolves with the seq the pump parked at
    /// (assert it equals `after_seq`); dropping/sending on `release` lets
    /// the pump continue.
    #[cfg(any(test, feature = "test-faults"))]
    #[doc(hidden)]
    pub async fn test_arm_follow_park(
        &self,
        after_seq: i64,
    ) -> (
        tokio::sync::oneshot::Receiver<i64>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.state.follow_parks.arm(after_seq).await
    }

    /// c-cardsettle-fu2 r1 item 4: arm the park AFTER the delete DB job
    /// commits (before publication/cleanup).
    #[cfg(any(test, feature = "test-faults"))]
    #[doc(hidden)]
    pub async fn test_arm_post_delete_park(
        &self,
        instance_id: &str,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.state
            .operation_parks
            .arm_post_delete(instance_id)
            .await
    }

    /// c-cardsettle-fu2 r4 item 3: number of settlement notices
    /// `broadcast_settlement` has actually put on the bus for
    /// `interaction_id`. A parked post-delete must keep this at 0 until the
    /// park is released; keyed by interaction so unrelated background
    /// settles (host sweeps, daemons) do not move the count.
    #[cfg(any(test, feature = "test-faults"))]
    #[doc(hidden)]
    pub fn test_settlement_notice_count(&self, interaction_id: &str) -> u64 {
        self.state
            .test_settlement_counts
            .lock()
            .unwrap()
            .get(interaction_id)
            .copied()
            .unwrap_or(0)
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
            .mutate_worker(
                worker_id.to_string(),
                move |row| {
                    if let Some(watch) = row.watch.as_mut() {
                        watch.last_activity_at = Some(stamp);
                    }
                    Ok(())
                },
                crate::agent_scope::CallerAuthority::internal(),
            )
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
        nodes: crate::transport::ConnectedNodes::boot(&store)
            .await
            .map_err(|err| anyhow::anyhow!("gate carrier boot epoch: {err}"))?,
        bus: Bus::with_capacity(config.follow_buffer_events),
        settlement_bus: {
            // Settlements are rare control notices; a small capacity is plenty
            // and keeps a lagging follower from skipping the terminal state.
            let (tx, _rx) = tokio::sync::broadcast::channel(64);
            tx
        },
        settlement_publish: Arc::new(tokio::sync::Mutex::const_new(())),
        #[cfg(any(test, feature = "test-faults"))]
        follow_parks: crate::ws::FollowParks::default(),
        #[cfg(any(test, feature = "test-faults"))]
        operation_parks: crate::ws::OperationParks::default(),
        #[cfg(any(test, feature = "test-faults"))]
        test_settlement_counts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
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
        gate_decision_guards: crate::gatequeue::GateDecisionGuards::new(),
        fences: crate::fence::FenceState::default(),
    };
    // D-057 §7.7: the send gate's map survives a Hub restart through the
    // lineages table; deliveries are re-sent on hello and by the tick.
    crate::fence::load_map(&state).await;
    if let Err(err) = state
        .publish_settlement_unit(async {
            Ok(state
                .store
                .expire_lost_hosts(config.host_lost_grace_ms)
                .await?
                .1)
        })
        .await
    {
        tracing::error!(error = %err, "startup host-lost sweep failed");
    }
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
                    let sweep = reaper_state
                        .publish_settlement(async {
                            reaper_state
                                .store
                                .expire_lost_hosts(config.host_lost_grace_ms)
                                .await
                        })
                        .await;
                    match sweep {
                        Ok(swept) => {
                            if swept > 0 {
                                tracing::debug!(swept, "host-lost sweep marked contact loss");
                            }
                        }
                        Err(err) => tracing::error!(error = %err, "host-lost sweep failed"),
                    }
                    expire_stale_requested(&reaper_state, config.requested_grace_ms).await;
                    // Off the loop: this select also polls the HTTP server,
                    // so the sweep must never hold it up.
                    let sweep_state = reaper_state.clone();
                    tokio::spawn(async move {
                        crate::fence::sweep_deliveries(&sweep_state).await;
                    });
                    crate::gatequeue::tick(&reaper_state).await;
                }
            }
        }
    });
    // r13 items 1/2: bounded, occupancy-preserving reconciliation of granted
    // Landing rows against the remote once carriers are reachable (independent
    // of the 1 s schedule tick so a remote probe never stalls dispatch).
    crate::gatequeue::spawn_landing_reconciler(&state);
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
    let expired = match state
        .publish_settlement(state.store.expire_stale_requested(window_ms))
        .await
    {
        Ok(outcome) => outcome,
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
        .merge(memory::routes())
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
        .merge(digest::routes())
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
