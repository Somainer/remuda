//! Project gate queue; coordinator hierarchy batch 6 (co-lanes), D-034.
//!
//! Verify jobs run in parallel across the project's lanes; land jobs are
//! serialized per project (`global-cas`: re-verify onto the current main,
//! compare-and-swap push, one re-verify when main moved). One job runs per
//! lane, queue order is FIFO per project, cancel is supported. The Hub owns
//! ordering and persistence; the lane host's Node owns the checkout and runs
//! the consumed `remuda merge --gate` CLI via `gate.run`.
//!
//! Every transition is journaled to `audit_log` (subject = job id, project id
//! in the detail), so `remuda watch` / `report` render gate rows without a
//! second event channel; live steps arrive as `gate.event` Node RPCs.
//!
//! A failed run's bounded log (last 400 lines + extracted summary) lands in
//! `gate_log_objects` as an `obj_…` row; the job keeps `failedStep`, the
//! one-line `reason` and `logObjectId` only. `GET /v1/gate/logs/{gjb|obj}`
//! reads the evidence (evidence: docs/design/evidence/gate-log-1.md).

use crate::AppState;
use crate::agent_scope::{caller, caller_project_scope, require_grant, require_operator};
use crate::auth::require_origin;
use crate::error::HubError;
use crate::http::map_store;
use crate::store::Store;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use remuda_protocol::{
    GateJob, GateJobId, GateJobState, GateMode, GateRunParams, GateRunResult, GateWebMode,
    GrantVerb, ProjectId, Timestamp,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// Total verify+land attempts for one land job (initial + CAS reverifies).
const MAX_LAND_ATTEMPTS: u32 = 3;
/// Gate run wall-clock budget the Hub keeps its RPC open (the Node enforces
/// its own cap too).
const GATE_RUN_TIMEOUT: Duration = Duration::from_secs(90 * 60);
/// Scheduler tick (also drives the host-lost reaper sweep).
pub(crate) const SCHEDULE_TICK_MILLIS: u64 = 1000;
/// How long a bounded gate-failure log stays fetchable.
const GATE_LOG_TTL_SECS: i64 = 30 * 24 * 60 * 60;
/// Budget for the home host's fetch-and-push land.
const HOME_LAND_TIMEOUT: Duration = Duration::from_secs(12 * 60);
/// Minimum spacing between read-only remote probes for one Landing row; the
/// configured interval is clamped to this floor so a misconfigured tiny value
/// cannot hammer a remote with fetches.
const MIN_LANDING_PROBE_INTERVAL_MS: u64 = 250;
/// Budget for dropping a job's pinned refs on a lane.
const UNPIN_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the pinned-ref retention sweep runs.
const REF_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Upper bound for one serialized run log; the Node bounds smaller.
const GATE_LOG_MAX_BYTES: usize = 512 * 1024;

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS gate_jobs (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            state TEXT NOT NULL,
            lane_id TEXT,
            queued_at TEXT NOT NULL,
            doc_json TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS gate_jobs_project ON gate_jobs(project_id);
         CREATE INDEX IF NOT EXISTS gate_jobs_state ON gate_jobs(state);
         CREATE INDEX IF NOT EXISTS gate_jobs_order ON gate_jobs(project_id, queued_at);
         CREATE TABLE IF NOT EXISTS gate_log_objects (
            id TEXT PRIMARY KEY,
            job_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            bytes BLOB NOT NULL,
            byte_len INTEGER NOT NULL,
            digest TEXT NOT NULL,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS gate_log_objects_job ON gate_log_objects(job_id);
         CREATE INDEX IF NOT EXISTS gate_log_objects_expires ON gate_log_objects(expires_at);",
    )?;
    // D-057 §7.1: the Hub-only authenticating device id lives in its own
    // column — GateJob.initiator_device_id is skip_serializing in the doc,
    // so a doc-only store would always claim with None and skip the device
    // clause. API responses keep skipping the field.
    crate::store::ensure_column(conn, "gate_jobs", "initiator_device_id", "TEXT")?;
    // r12 item 2: the authenticated carrier SESSION the dispatch was sent on
    // (the Hub node-link generation minted at hello). The land grant and the
    // pre-run authorize fence compare the inbound socket's session against
    // this, so a paused/old frame riding a replacement session after a
    // reconnect can neither execute nor land. Hub-only: never on the wire.
    crate::store::ensure_column(conn, "gate_jobs", "dispatch_session", "INTEGER")?;
    // r13 item 1: when the Running → Landing grant committed, so a Hub restart
    // can bound how long an unconfirmed Landing may wait before the push is
    // provably over budget. Hub-only: never on the wire.
    crate::store::ensure_column(conn, "gate_jobs", "landing_at", "TEXT")?;
    // r13 item 1: last read-only remote probe of a Landing row (throttle).
    crate::store::ensure_column(conn, "gate_jobs", "landing_probed_at", "TEXT")?;
    // r14 R3: monotonic Hub BOOT epoch backing the never-repeating carrier
    // session identity. One row, allocated atomically at every boot; the value
    // packs into the session id's high 32 bits.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS hub_kv (
            key TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        )",
        [],
    )?;
    Ok(())
}

impl Store {
    /// r15 item 8 (authority continuity): rebind the host's in-flight Landing
    /// rows to a freshly connected SAME-BOOT carrier session. A same-epoch
    /// reconnect is that boot's continuation of authority for rows dispatched
    /// before the socket dropped; the landing probe/settle predicates require
    /// the CURRENT session, so the persisted binding must follow it. Rows from
    /// an OLDER boot epoch are deliberately left bound to their stale session
    /// (the post-restart reconciler matches those by their persisted id).
    pub(crate) async fn rebind_same_boot_landing_sessions(
        &self,
        host_id: &str,
        new_session: u64,
        boot_epoch: u64,
    ) -> Result<u64, crate::store::StoreError> {
        let host_id = host_id.to_owned();
        self.run_named("rebind_same_boot_landing_sessions", move |conn| {
            let tx = crate::store::immediate_tx(conn)?;
            let affected = tx.execute(
                "UPDATE gate_jobs
                    SET dispatch_session = ?1
                  WHERE state = 'landing'
                    AND json_extract(doc_json, '$.hostId') = ?2
                    AND dispatch_session IS NOT NULL
                    AND dispatch_session <> ?1
                    AND (dispatch_session >> 32) = ?3",
                params![
                    i64::try_from(new_session).unwrap_or(0),
                    host_id,
                    i64::try_from(boot_epoch).unwrap_or(0)
                ],
            )?;
            tx.commit()?;
            Ok(u64::try_from(affected).unwrap_or(0))
        })
        .await
    }

    /// r14 R3: atomically allocate this Hub process's boot epoch (the high 32
    /// bits of every gate carrier session identity) in ONE immediate
    /// transaction that both validates against retained sessions and claims the
    /// next value, so a crash or a partial restore can never roll the counter
    /// back.
    ///
    /// Fail closed: before allocating, the persisted value is checked against
    /// every RETAINED carrier session (the `dispatch_session` columns of gate
    /// jobs that survive this restart):
    /// * no kv row AND no retained epochs → first boot, allocate epoch 1;
    /// * a kv row present and >= the maximum retained epoch → allocate kv + 1;
    /// * a kv row missing WITH retained sessions, or a kv SMALLER than a
    ///   retained epoch (a cloned/partially restored/rolled-back database) →
    ///   [`StoreError::GateEpochRegressed`]: minting here could repeat a live
    ///   session identity, so the Hub refuses to mint (and fails to start).
    pub(crate) async fn next_gate_session_boot_epoch(
        &self,
    ) -> Result<u64, crate::store::StoreError> {
        self.run_named("next_gate_session_boot_epoch", |conn| {
            let tx = crate::store::immediate_tx(conn)?;
            // Highest epoch embedded in any retained carrier session.
            let retained: Option<i64> = tx
                .query_row("SELECT MAX(dispatch_session) FROM gate_jobs", [], |row| {
                    row.get(0)
                })
                .optional()?
                .flatten();
            let retained_epoch = retained.map(|session| (session >> 32) as u64).unwrap_or(0);
            // r16 item 9 / r17 item 7: sessions are stored as POSITIVE signed
            // i64, so the packed (epoch<<32 | counter) id must fit: an epoch
            // >= 2^31 packs to a negative/zero i64 (collapsing the persisted
            // grant session), and >= 2^32 also shifts out of the 32-bit field.
            // A negative retained row (high bit already set) sign-extends into
            // a huge epoch and is refused too.
            if retained_epoch >= MAX_GATE_SESSION_BOOT_EPOCH {
                return Err(crate::store::StoreError::GateEpochRegressed(format!(
                    "a retained gate carrier session embeds boot epoch {retained_epoch}, at/above \
                     the signed 2^31 session-id limit; refusing to mint session ids"
                )));
            }
            // Current persisted allocation epoch, if any.
            let current: Option<i64> = tx
                .query_row(
                    "SELECT value FROM hub_kv WHERE key = 'gate_session_boot_epoch'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let next = match current {
                None if retained_epoch == 0 => 1,
                None => {
                    return Err(crate::store::StoreError::GateEpochRegressed(format!(
                        "retained gate carrier sessions require boot epoch >= {retained_epoch} but \
                         no gate_session_boot_epoch row exists; refusing to mint session ids"
                    )));
                }
                Some(value) => {
                    let stored = value.max(0) as u64;
                    if stored < retained_epoch {
                        return Err(crate::store::StoreError::GateEpochRegressed(format!(
                            "gate_session_boot_epoch={stored} is older than a retained carrier \
                             session epoch {retained_epoch}; refusing to mint session ids"
                        )));
                    }
                    // r16 item 9 / r17 item 7: never allocate an epoch whose
                    // packed session id would not fit a positive signed i64 —
                    // fail closed.
                    if stored >= MAX_GATE_SESSION_BOOT_EPOCH {
                        return Err(crate::store::StoreError::GateEpochRegressed(format!(
                            "gate_session_boot_epoch={stored} is at or above the signed 2^31 \
                             session-id limit; refusing to mint session ids"
                        )));
                    }
                    let next = stored.max(retained_epoch).saturating_add(1);
                    if next >= MAX_GATE_SESSION_BOOT_EPOCH {
                        return Err(crate::store::StoreError::GateEpochRegressed(format!(
                            "the next gate session boot epoch would be {next}, at or above the \
                             signed 2^31 session-id limit; refusing to mint aliasing session ids"
                        )));
                    }
                    next
                }
            };
            tx.execute(
                "INSERT INTO hub_kv(key, value) VALUES ('gate_session_boot_epoch', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![i64::try_from(next).expect("bounded below 2^31 by the checks above")],
            )?;
            tx.commit()?;
            Ok(next)
        })
        .await
    }
}

/// Exclusive upper bound for a gate carrier boot epoch. The session id is packed
/// as `(epoch << 32) | counter` and persisted in a POSITIVE signed SQLite
/// INTEGER (i64): the largest representable value is `2^63 - 1`, so the epoch
/// must stay `< 2^31` (at `2^31 - 1`, counter `2^32 - 1` reaches `2^63 - 1`).
/// An epoch in `[2^31, 2^32)` would pack to a NEGATIVE/zero i64 on storage,
/// collapsing the persisted grant session to NULL/0 and aliasing other
/// sessions; `>= 2^32` also shifts the epoch out of the 32-bit field
/// (r16 item 9; tightened to 2^31 in r17 item 7).
const MAX_GATE_SESSION_BOOT_EPOCH: u64 = 1 << 31;

/// Routes for the project gate queue.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/projects/{id}/gate",
            get(list_project_jobs).post(enqueue_job),
        )
        .route("/v1/projects/{id}/gate/jobs/{jobId}", get(get_project_job))
        .route(
            "/v1/projects/{id}/gate/jobs/{jobId}/cancel",
            post(cancel_project_job),
        )
        .route(
            "/v1/projects/{id}/gate/jobs/{jobId}/resolve",
            post(resolve_project_job),
        )
        .route(
            "/v1/projects/{id}/gate/jobs/{jobId}/resolve-evidence",
            post(resolve_evidence_project_job),
        )
        .route("/v1/gate/jobs", get(list_all_jobs))
        .route("/v1/gate/logs/{id}", get(get_gate_log))
}

// ── request shapes ─────────────────────────────────────────────────────────

/// Test-only barrier between evidence persist and the settlement mutate.
/// Two modes:
/// * a GLOBAL release frees every parked arrival ([`SimpleBarrier::release`]);
/// * an INDEPENDENT per-key gate
///   ([`SimpleBarrier::release_key`]) frees exactly one arrival parked under
///   that key — r21 item 6 lets a race test release handler A, verify A's
///   settlement with B still parked, then release B.
///
/// The production wait site keys by THIS arrival's persisted object id (empty
/// key when it supplied no log); distinct results have distinct
/// content-addressed keys.
#[doc(hidden)]
#[cfg(any(test, feature = "test-faults"))]
pub struct SimpleBarrier {
    armed: std::sync::atomic::AtomicBool,
    parked: std::sync::atomic::AtomicUsize,
    notify: tokio::sync::Notify,
    slots: tokio::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<BarrierSlot>>>,
}

#[cfg(any(test, feature = "test-faults"))]
struct BarrierSlot {
    released: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

#[cfg(any(test, feature = "test-faults"))]
impl SimpleBarrier {
    pub(crate) fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(false),
            parked: std::sync::atomic::AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
            slots: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
    pub fn arm(&self) {
        self.parked.store(0, std::sync::atomic::Ordering::SeqCst);
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        // arm() precedes every arrival, so no waiter holds the map: drop any
        // slots from the previous arm without going async.
        if let Ok(mut slots) = self.slots.try_lock() {
            slots.clear();
        }
    }
    /// Park THIS arrival, keyed by its persisted object id, until either the
    /// global release or its own [`SimpleBarrier::release_key`].
    pub(crate) async fn wait_key(&self, key: &str) {
        if !self.armed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let slot = {
            let mut slots = self.slots.lock().await;
            slots
                .entry(key.to_owned())
                .or_insert_with(|| {
                    std::sync::Arc::new(BarrierSlot {
                        released: std::sync::atomic::AtomicBool::new(false),
                        notify: tokio::sync::Notify::new(),
                    })
                })
                .clone()
        };
        self.parked
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        loop {
            if !self.armed.load(std::sync::atomic::Ordering::SeqCst)
                || slot.released.load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            let released = slot.notify.notified();
            let global = self.notify.notified();
            tokio::pin!(released, global);
            tokio::select! {
                _ = released => {}
                _ = global => {}
            }
        }
    }
    pub async fn wait_parked(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if self.parked.load(std::sync::atomic::Ordering::SeqCst) >= n {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "only {} arrivals parked (wanted {})",
                self.parked.load(std::sync::atomic::Ordering::SeqCst),
                n
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    /// Release every parked arrival.
    pub fn release(&self) {
        self.armed.store(false, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    /// Release exactly the arrival parked under `key`; other arrivals stay
    /// parked.
    pub async fn release_key(&self, key: &str) {
        let slot = self.slots.lock().await.get(key).cloned();
        if let Some(slot) = slot {
            slot.released
                .store(true, std::sync::atomic::Ordering::Release);
            slot.notify.notify_waiters();
        }
    }
}

/// Latching Notify: a release before the park is not lost; every wait is
/// bounded.
#[doc(hidden)]
#[cfg(any(test, feature = "test-faults"))]
pub struct GateApplyBarrier {
    /// When false arrivals pass straight through (every other test).
    armed: std::sync::atomic::AtomicBool,
    parked: std::sync::atomic::AtomicUsize,
    /// Signalled each time an arrival parks (no sleeps in wait_parked).
    parked_notify: tokio::sync::Notify,
    fired: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
    /// apply_result calls fully returned, and their notify.
    completed: std::sync::atomic::AtomicUsize,
    completed_notify: tokio::sync::Notify,
    /// fail_rpc_inner calls fully returned (every gate.run RPC error/timeout),
    /// and their notify — lets tests latch on the stale-RPC-failure decision
    /// instead of a stability window.
    rpc_completed: std::sync::atomic::AtomicUsize,
    rpc_completed_notify: tokio::sync::Notify,
    /// on_event notifications fully handled (a step/log/phase event has run
    /// its writer), and their notify — lets tests latch event rejection
    /// instead of holding a stability window.
    event_completed: std::sync::atomic::AtomicUsize,
    event_completed_notify: tokio::sync::Notify,
}

#[cfg(any(test, feature = "test-faults"))]
impl GateApplyBarrier {
    pub(crate) fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(false),
            parked: std::sync::atomic::AtomicUsize::new(0),
            parked_notify: tokio::sync::Notify::new(),
            fired: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
            completed: std::sync::atomic::AtomicUsize::new(0),
            completed_notify: tokio::sync::Notify::new(),
            rpc_completed: std::sync::atomic::AtomicUsize::new(0),
            rpc_completed_notify: tokio::sync::Notify::new(),
            event_completed: std::sync::atomic::AtomicUsize::new(0),
            event_completed_notify: tokio::sync::Notify::new(),
        }
    }

    /// Start gating arrivals (resets counters/fire).
    pub fn arm(&self) {
        self.parked.store(0, std::sync::atomic::Ordering::SeqCst);
        self.completed.store(0, std::sync::atomic::Ordering::SeqCst);
        self.rpc_completed
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.event_completed
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.fired.store(false, std::sync::atomic::Ordering::SeqCst);
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Release all arrivals parked at the boundary.
    pub fn release(&self) {
        if self
            .fired
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            self.notify.notify_waiters();
        }
    }

    /// Wait until `n` arrivals have parked. Latch-driven (no sleeps): each
    /// parking arrival notifies, and the notified future is constructed
    /// before the counter read.
    pub async fn wait_parked(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let notified = self.parked_notify.notified();
            if self.parked.load(std::sync::atomic::Ordering::SeqCst) >= n {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!(
                    "only {} arrivals parked (wanted {n})",
                    self.parked.load(std::sync::atomic::Ordering::SeqCst)
                );
            }
        }
    }

    /// Park until released. The notified future is created BEFORE the flag
    /// read, so a release that landed first is observed.
    pub async fn wait(&self) {
        // Not armed: this test does not use the barrier.
        if !self.armed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        self.parked
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.parked_notify.notify_waiters();
        let notified = self.notify.notified();
        if self.fired.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        if tokio::time::timeout(Duration::from_secs(10), notified)
            .await
            .is_err()
        {
            panic!("gate apply barrier never released");
        }
    }

    /// Count an apply_result call that has FINISHED its whole path (r11 item 7:
    /// tests wait for both arrivals before asserting).
    pub fn note_completed(&self) {
        self.completed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.completed_notify.notify_waiters();
    }

    /// Wait until `n` apply_result calls have fully returned (no sleeps).
    pub async fn wait_completed(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let notified = self.completed_notify.notified();
            if self.completed.load(std::sync::atomic::Ordering::SeqCst) >= n {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!(
                    "only {} apply_result calls completed (wanted {n})",
                    self.completed.load(std::sync::atomic::Ordering::SeqCst)
                );
            }
        }
    }

    /// Wait until `n` fail_rpc (gate.run RPC error/timeout) handlers have fully
    /// returned (no sleeps). Used to latch the "stale RPC failure stood down"
    /// production decision instead of watching a stability time window.
    pub async fn wait_rpc_completed(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let notified = self.rpc_completed_notify.notified();
            if self.rpc_completed.load(std::sync::atomic::Ordering::SeqCst) >= n {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!(
                    "only {} fail_rpc calls completed (wanted {n})",
                    self.rpc_completed.load(std::sync::atomic::Ordering::SeqCst)
                );
            }
        }
    }

    /// Wait until `n` gate.event notifications have been fully handled.
    pub async fn wait_event_completed(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let notified = self.event_completed_notify.notified();
            if self
                .event_completed
                .load(std::sync::atomic::Ordering::SeqCst)
                >= n
            {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!(
                    "only {} gate.event calls completed (wanted {n})",
                    self.event_completed
                        .load(std::sync::atomic::Ordering::SeqCst)
                );
            }
        }
    }

    /// Number of gate.event notifications fully handled so far.
    #[cfg(any(test, feature = "test-faults"))]
    pub fn event_completed_count(&self) -> usize {
        self.event_completed
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Number of fail_rpc handlers fully returned so far.
    #[cfg(any(test, feature = "test-faults"))]
    pub fn rpc_completed_count(&self) -> usize {
        self.rpc_completed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnqueueBody {
    branch: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    web: Option<String>,
    #[serde(default)]
    lane_id: Option<String>,
    #[serde(default)]
    then_command: Option<String>,
    #[serde(default)]
    keep_logs: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AllJobsQuery {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    limit: Option<u32>,
}

// ── handlers ───────────────────────────────────────────────────────────────

/// `POST /v1/projects/{id}/gate` — enqueue a verify or land job.
async fn enqueue_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Json(body): Json<EnqueueBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    let project_id = ProjectId::try_from(project.clone())
        .map_err(|error| HubError::BadRequest(error.to_string()))?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(&project) {
        return Err(HubError::Forbidden);
    }
    let mode = match body.mode.as_deref() {
        None | Some("verify") => GateMode::Verify,
        Some("land") => GateMode::Land,
        Some(other) => {
            return Err(HubError::BadRequest(format!(
                "mode must be verify|land, got {other:?}"
            )));
        }
    };
    require_grant(
        &state,
        &device,
        if mode == GateMode::Land {
            GrantVerb::Land
        } else {
            GrantVerb::Dispatch
        },
    )
    .await?;
    let project_doc = state
        .store
        .get_project(project)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    let web = match body.web.as_deref() {
        None | Some("auto") => GateWebMode::Auto,
        Some("always") => GateWebMode::Always,
        Some("never") => GateWebMode::Never,
        Some(other) => {
            return Err(HubError::BadRequest(format!(
                "web must be auto|always|never, got {other:?}"
            )));
        }
    };
    let branch = body.branch.trim();
    validate_branch(branch)?;
    if let Some(lane_id) = &body.lane_id
        && !project_doc
            .gate
            .lanes
            .iter()
            .any(|lane| &lane.id == lane_id)
    {
        return Err(HubError::BadRequest(format!(
            "project has no lane {lane_id:?}"
        )));
    }
    if body.then_command.as_deref().is_some_and(str::is_empty) {
        return Err(HubError::BadRequest(
            "then command must not be empty".into(),
        ));
    }
    let now = now();
    // D-057 §7.1: the gate job doc carries the caller's stamped initiator and
    // device id; the claim re-checks them (§7.3). insert refuses a fenced one.
    let (gate_initiator, gate_device_id) =
        crate::agent_scope::initiator_and_device(&state, &device).await?;
    let job = GateJob {
        id: GateJobId::new(),
        project_id,
        branch: branch.to_owned(),
        mode,
        web,
        lane_id: body.lane_id,
        requested_by: device.id.clone(),
        initiator: gate_initiator,
        initiator_device_id: gate_device_id,
        state: GateJobState::Queued,
        steps: Vec::new(),
        host_id: None,
        queued_at: ts(&now),
        started_at: None,
        finished_at: None,
        cancel_requested_at: None,
        home_land_in_flight: false,
        base_sha: None,
        head_sha: None,
        merge_sha: None,
        merge_ref: None,
        current_main_sha: None,
        error: None,
        failed_step: None,
        reason: None,
        log_object_id: None,
        keep_logs: body.keep_logs.unwrap_or(false),
        attempts: 0,
        then_command: body.then_command,
        then_output: None,
        landing_session_epoch: None,
        landing_outcome_unknown: false,
        evidence_uncertain: None,
    };
    state.store.insert_gate_job(&job).await.map_err(map_store)?;
    journal(&state, &device.id, "gate.queued", &job).await;
    schedule_once(&state);
    Ok(Json(json!(job)))
}

/// `GET /v1/projects/{id}/gate`
async fn list_project_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, HubError> {
    let device = caller(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(&project) {
        return Err(HubError::Forbidden);
    }
    let mut jobs = state
        .store
        .list_gate_jobs(Some(&project))
        .await
        .map_err(map_store)?;
    filter_jobs(
        &mut jobs,
        query.state.as_deref(),
        query.branch.as_deref(),
        query.limit,
    );
    Ok(Json(json!({ "items": jobs, "nextCursor": null })))
}

/// `GET /v1/gate/jobs[?active=1]` — cross-project view for `remuda watch`.
async fn list_all_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AllJobsQuery>,
) -> Result<Json<Value>, HubError> {
    let device = caller(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let mut jobs = state.store.list_gate_jobs(None).await.map_err(map_store)?;
    jobs.retain(|job| scope.allows_project(job.project_id.as_id().as_str()));
    if query.active == Some(true) {
        jobs.retain(|job| {
            matches!(
                job.state,
                GateJobState::Queued | GateJobState::Running | GateJobState::Landing
            )
        });
    }
    if let Some(state_name) = query.state.as_deref() {
        jobs.retain(|job| job.state.as_str() == state_name);
    }
    if let Some(limit) = query.limit {
        jobs.truncate(limit as usize);
    }
    Ok(Json(json!({ "items": jobs, "nextCursor": null })))
}

/// `GET /v1/projects/{id}/gate/jobs/{jobId}`
async fn get_project_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((project, job_id)): Path<(String, String)>,
) -> Result<Json<Value>, HubError> {
    let job = resolve_job(&state, &headers, &project, &job_id).await?;
    Ok(Json(json!(job)))
}

/// `POST /v1/projects/{id}/gate/jobs/{jobId}/cancel`
async fn cancel_project_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((project, job_id)): Path<(String, String)>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(&project) {
        return Err(HubError::Forbidden);
    }
    let job = state
        .store
        .get_gate_job(&job_id)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if job.project_id.as_id().as_str() != project {
        return Err(HubError::NotFound);
    }
    require_grant(
        &state,
        &device,
        if job.mode == GateMode::Land {
            GrantVerb::Land
        } else {
            GrantVerb::Dispatch
        },
    )
    .await?;
    // D-057 §7.3: the canceller's authority is checked INSIDE the writer job
    // that moves the row — before any gate.cancel frame — so a request
    // authenticated before a fence cannot cancel after it. Scheduler-driven
    // cancels stay unchecked (a started job's follow-ons are never fenced).
    //
    // The branch decision uses the state observed INSIDE the writer, not the
    // pre-read above: the 1 s scheduler tick can claim a queued job (or a
    // running job can finish) between that read and the writer job, so a
    // frame must be sent exactly when the writer saw Running/Canceling.
    let authority = crate::agent_scope::CallerAuthority::for_device(&state, &device).await?;
    let outcome = state
        .store
        .request_gate_job_cancel(&job_id, authority)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    match outcome {
        GateJobCancelOutcome::QueuedCanceled(updated) => {
            // The queued transition is the terminal canceled write; a fenced
            // writer cannot reach here, and no Node frame is owed.
            journal(&state, &device.id, "gate.canceled", &updated).await;
            drop_job_refs(&state, &updated).await;
            Ok(Json(json!(updated)))
        }
        GateJobCancelOutcome::CancelRequested(updated) => {
            if let (Some(host), Some(lane)) = (&updated.host_id, &updated.lane_id) {
                // §7.5: gate.cancel is a follow-on of the admitted (possibly
                // Node-started) job: the jobId is its opId. The frame carries
                // no initiator — the Node admits it as a continuation, and the
                // Hub-side authority of the cancel itself was checked in the
                // writer above.
                let params = json!({
                    "jobId": updated.id.as_id().as_str(),
                    "laneId": lane,
                    // The dispatch generation being canceled: the Node
                    // refuses a same/older generation gate.run even if that
                    // frame is delayed past the Hub's grace settlement.
                    "generation": updated.attempts,
                });
                // Best effort: the running tick also fails the job if the
                // Node is gone.
                if let Ok(Some(_)) = state
                    .nodes
                    .call(
                        host.as_id().as_str(),
                        "gate.cancel",
                        params,
                        Duration::from_secs(10),
                    )
                    .await
                {
                    journal(&state, &device.id, "gate.canceling", &updated).await;
                }
            }
            Ok(Json(json!(updated)))
        }
        GateJobCancelOutcome::Landing(job) => {
            tracing::info!(job_id = job.id.as_id().as_str(), "cancel after land grant");
            Err(HubError::Conflict(
                "job landing in progress; cancel cannot stop the granted push".to_string(),
            ))
        }
        GateJobCancelOutcome::Terminal(job) => Err(HubError::Conflict(format!(
            "job already {}",
            job.state.as_str()
        ))),
    }
}

/// r14 R1 / r15 item 4 — body of the OPERATOR Landing-resolution request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveLandingRequest {
    /// `failed` — the operator attests the push did not land; finish Failed.
    /// `requeue` — re-enter the queue for a fresh verify+land attempt.
    resolution: String,
    /// The exact attempt ordinal the operator is resolving.
    expected_attempts: u32,
    /// The Running → Landing grant epoch the operator's view carried.
    expected_session_epoch: String,
    /// The exact granted merge sha the operator is resolving.
    expected_merge_sha: String,
    /// Mandatory human rationale; persisted on the row and journaled.
    rationale: String,
}

/// `POST /v1/projects/{id}/gate/jobs/{jobId}/resolve`
///
/// The explicit OPERATOR-ONLY escape hatch for a Landing whose outcome cannot
/// be established (it is parked in the outcome-unknown Landing state holding
/// its lane). Agent callers are refused outright; the request must pin the
/// exact granted (attempt, grant epoch, merge) the operator saw and carry a
/// persisted rationale. An established remote fact always wins over the
/// operator: with the lane carrier live, the Hub takes the same per-host
/// authority guard the reconciler uses and makes one final read-only probe —
/// a probe that establishes the merge on the remote settles LANDED, and the
/// operator's failed/requeue is discarded (r15 item 4).
async fn resolve_project_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((project, job_id)): Path<(String, String)>,
    Json(request): Json<ResolveLandingRequest>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    // r15 item 4: Human/Bot operators only. An agent caller (even one holding
    // the land grant) is not an operator and cannot discard an unknown Landing.
    require_operator(&state, &headers).await?;
    let device = caller(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(&project) {
        return Err(HubError::Forbidden);
    }
    let requeue = match request.resolution.as_str() {
        "failed" => false,
        "requeue" => true,
        other => {
            return Err(HubError::BadRequest(format!(
                "resolution must be \"failed\" or \"requeue\", got {other:?}"
            )));
        }
    };
    let rationale = request.rationale.trim();
    if rationale.is_empty() {
        return Err(HubError::BadRequest(
            "an operator Landing resolution requires a non-empty rationale".to_string(),
        ));
    }
    if request.expected_merge_sha.trim().is_empty() || request.expected_session_epoch.is_empty() {
        return Err(HubError::BadRequest(
            "an operator Landing resolution must name the expected attempt, grant epoch and merge"
                .to_string(),
        ));
    }
    let job = state
        .store
        .get_gate_job(&job_id)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if job.project_id.as_id().as_str() != project {
        return Err(HubError::NotFound);
    }
    if job.state != GateJobState::Landing || !job.landing_outcome_unknown {
        return Err(HubError::Conflict(
            "only a Landing parked with an unknown outcome accepts an operator resolution"
                .to_string(),
        ));
    }
    // Final read-only remote check under the per-host authority guard: an
    // ESTABLISHED landed fact always beats the operator's failed/requeue. The
    // guard pins the live carrier session across probe + commit exactly like
    // the periodic reconciler (r15 item 3), so a concurrent takeover cannot
    // make the probe land on a different session. A carrier-less host (or a
    // home-pushed job whose group liveness is unattestable) falls through to
    // the explicit operator decision.
    if let Some(lane_host) = landing_lane_host(&state, &job).await {
        let guard = state
            .nodes
            .host_authority(lane_host.as_id().as_str())
            .await
            .read_owned()
            .await;
        if let Some(responder_session) = state.nodes.generation_of(lane_host.as_id().as_str()).await
        {
            // Only authoritative for a lane-pushed job (the probe's dead-group
            // attestation is the lane's own). The Landed fact is valid either
            // way.
            if matches!(
                probe_landing_outcome(&state, &job).await,
                LandingProbe::Landed
            ) {
                let grant_session = state
                    .store
                    .gate_job_dispatch_session(&job_id)
                    .await
                    .ok()
                    .flatten();
                let expected_session =
                    probe_expected_session(&state, grant_session, responder_session);
                settle_landing_landed(
                    &state,
                    &job,
                    expected_session,
                    responder_session,
                    &lane_host,
                )
                .await;
                let landed = state
                    .store
                    .get_gate_job(&job_id)
                    .await
                    .map_err(map_store)?
                    .ok_or(HubError::NotFound)?;
                if landed.state == GateJobState::Landed {
                    return Ok(Json(json!(landed)));
                }
                // r16 item 7: the exact merge was OBSERVED on the remote but
                // the snapshot-settling writer refused (the live row advanced
                // under the probe — a newer attempt/grant). The operator's
                // failed/requeue must NEVER run from here (it would discard
                // the established fact and requeue/fail a push that landed).
                // Keep the Landing and hand the operator a 409 carrying the
                // current state, exactly like any authority mismatch.
                drop(guard);
                return Err(HubError::Conflict(format!(
                    "the remote probe established the exact granted merge landed, but the live row \
                     no longer matches that snapshot (now {}); the Landing was kept — refresh and \
                     resolve the current attempt",
                    landed.state.as_str()
                )));
            }
        }
        drop(guard);
    }
    let operator = device.id.clone();
    let rationale = rationale.to_owned();
    let expected_attempts = request.expected_attempts;
    let expected_epoch = request.expected_session_epoch.clone();
    let expected_merge = request.expected_merge_sha.clone();
    // r20 item 6: validate the COMPLETE requested tuple (attempt, epoch,
    // merge) against a FRESH read BEFORE any abort side effect. A delayed frame
    // for attempt N while the job has advanced to an outcome-unknown attempt
    // N+1 used to call abort_landing_run with the CURRENT row first, so the
    // Node aborted/tombstoned N+1 and only afterwards received its 409. A stale
    // resolution now leaves with a 409 and sends NOTHING.
    let current = state
        .store
        .get_gate_job(&job_id)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if current.state != GateJobState::Landing || !current.landing_outcome_unknown {
        return Err(HubError::NotFound);
    }
    if current.attempts != expected_attempts
        || current.landing_session_epoch.as_deref() != Some(expected_epoch.as_str())
        || current.merge_sha.as_deref() != Some(expected_merge.as_str())
    {
        return Err(HubError::Conflict(
            "the live Landing is no longer the granted attempt/epoch/merge this resolution named; \
             refresh and resolve the current attempt"
                .to_string(),
        ));
    }
    // r19 item 1 / R6 + r20 item 6: stop the granted land group — carrying the
    // VALIDATED generation — and wait for the Node's unique owner to
    // kill/reap/release the lane BEFORE the terminal/requeue state becomes
    // visible, so a scheduler tick cannot hand the still-held lane to the next
    // dispatch between the row commit and the abort.
    abort_landing_run(&state, &current).await;
    let outcome = state
        .store
        .mutate_gate_job(&job_id, move |row, _conn| {
            if row.state != GateJobState::Landing || !row.landing_outcome_unknown {
                return None;
            }
            // r15 item 4: the writer validates the exact granted tuple the
            // operator claims to resolve. A paused frame targeting a newer
            // attempt (or a row whose grant/merge changed) refuses here.
            if row.attempts != expected_attempts
                || row.landing_session_epoch.as_deref() != Some(expected_epoch.as_str())
                || row.merge_sha.as_deref() != Some(expected_merge.as_str())
            {
                return None;
            }
            row.home_land_in_flight = false;
            row.landing_session_epoch = None;
            row.landing_outcome_unknown = false;
            row.evidence_uncertain = None;
            if requeue {
                row.state = GateJobState::Queued;
                row.started_at = None;
                row.host_id = None;
                row.lane_id = None;
                row.finished_at = None;
                row.current_main_sha = None;
                row.merge_ref = None;
                row.merge_sha = None;
                row.failed_step = None;
                row.reason = None;
                row.log_object_id = None;
                row.error = Some(format!(
                    "landing outcome unknown; re-queued by explicit operator resolution \
                     ({operator}): {rationale}"
                ));
            } else {
                row.state = GateJobState::Failed;
                row.finished_at = Some(now_ts());
                row.error = Some(format!(
                    "landing outcome unknown after the bounded recovery window; settled failed by \
                     explicit operator resolution ({operator}): {rationale}"
                ));
                row.reason = Some(format!(
                    "failed; landing outcome unknown, resolved by operator: {rationale}"
                ));
            }
            Some(())
        })
        .await
        .map_err(map_store)?;
    let Some(outcome) = outcome else {
        // Distinguish the two writer refusals for the caller.
        let current = state
            .store
            .get_gate_job(&job_id)
            .await
            .map_err(map_store)?
            .ok_or(HubError::NotFound)?;
        if current.state != GateJobState::Landing || !current.landing_outcome_unknown {
            return Err(HubError::NotFound);
        }
        return Err(HubError::Conflict(
            "the live Landing is no longer the granted attempt/epoch/merge this resolution named; \
             refresh and resolve the current attempt"
                .to_string(),
        ));
    };
    if requeue {
        journal(&state, &device.id, "gate.base-moved-requeue", &outcome).await;
        schedule_once(&state);
    } else {
        journal(&state, &device.id, "gate.failed", &outcome).await;
        drop_job_refs(&state, &outcome).await;
    }
    Ok(Json(json!(outcome)))
}

/// r23 R9 clarification — body of the OPERATOR evidence-uncertain resolution
/// request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveEvidenceRequest {
    /// `failed` — settle the job Failed with the operator's attestation (no
    /// definitive verdict is auto-accepted without its persisted evidence);
    /// `requeue` — release the occupancy and re-enter the queue for a fresh
    /// run.
    resolution: String,
    /// The attempt generation the operator saw parked as uncertain.
    expected_generation: u32,
    /// Mandatory human rationale; persisted on the row and journaled.
    rationale: String,
}

/// `POST /v1/projects/{id}/gate/jobs/{jobId}/resolve-evidence`
///
/// The explicit OPERATOR-ONLY escape hatch for an evidence-UNCERTAIN job: a
/// definitive verdict (e.g. Failed-with-log or Passed-with-keepLogs) whose
/// supplied run log could not be persisted on either of the Node's two
/// deliveries. The Node retains no copy and redelivers nothing, and the Hub
/// runs no retry scheduler, so the row parks in its pre-settlement state
/// holding its lane/land occupancy until an operator resolves it here or an
/// identical redelivery ever arrives (which settles idempotently and clears
/// the condition on its own). Agent callers are refused; the request must pin
/// the exact parked generation and carry a persisted rationale.
async fn resolve_evidence_project_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((project, job_id)): Path<(String, String)>,
    Json(request): Json<ResolveEvidenceRequest>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_operator(&state, &headers).await?;
    let device = caller(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    if !scope.allows_project(&project) {
        return Err(HubError::Forbidden);
    }
    let requeue = match request.resolution.as_str() {
        "failed" => false,
        "requeue" => true,
        other => {
            return Err(HubError::BadRequest(format!(
                "resolution must be \"failed\" or \"requeue\", got {other:?}"
            )));
        }
    };
    let rationale = request.rationale.trim();
    if rationale.is_empty() {
        return Err(HubError::BadRequest(
            "an operator evidence-uncertain resolution requires a non-empty rationale".to_string(),
        ));
    }
    let expected_generation = request.expected_generation;
    let job = state
        .store
        .get_gate_job(&job_id)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if job.project_id.as_id().as_str() != project {
        return Err(HubError::NotFound);
    }
    let condition = match &job.evidence_uncertain {
        Some(condition)
            if matches!(
                job.state,
                GateJobState::Running | GateJobState::Canceling | GateJobState::Landing
            ) =>
        {
            condition.clone()
        }
        _ => {
            return Err(HubError::Conflict(
                "only a job parked with an unpersisted-evidence uncertain condition accepts an \
                 evidence resolution"
                    .to_string(),
            ));
        }
    };
    if condition.generation != expected_generation {
        return Err(HubError::Conflict(
            "the live uncertain condition names a different attempt generation than this \
             resolution; refresh and resolve the current attempt"
                .to_string(),
        ));
    }
    let was_landing = job.state == GateJobState::Landing;
    // A Landing's granted land run has normally already settled its group by
    // the time a verdict is delivered, but terminate/tombstone the generation
    // exactly like the Landing operator resolve so an operator-failed Landing
    // cannot push after the fact.
    if was_landing {
        abort_landing_run(&state, &job).await;
    }
    let operator = device.id.clone();
    let rationale = rationale.to_owned();
    let outcome = state
        .store
        .mutate_gate_job(&job_id, move |row, _conn| {
            let Some(condition) = &row.evidence_uncertain else {
                return None;
            };
            if !matches!(
                row.state,
                GateJobState::Running | GateJobState::Canceling | GateJobState::Landing
            ) || condition.generation != expected_generation
            {
                return None;
            }
            row.home_land_in_flight = false;
            row.landing_session_epoch = None;
            row.landing_outcome_unknown = false;
            row.evidence_uncertain = None;
            if requeue {
                row.state = GateJobState::Queued;
                row.started_at = None;
                row.host_id = None;
                row.lane_id = None;
                row.finished_at = None;
                row.current_main_sha = None;
                row.merge_ref = None;
                row.merge_sha = None;
                row.failed_step = None;
                row.reason = None;
                row.log_object_id = None;
                row.error = Some(format!(
                    "evidence could not be persisted for a definitive verdict; re-queued by \
                     explicit operator resolution ({operator}): {rationale}"
                ));
            } else {
                row.state = GateJobState::Failed;
                row.finished_at = Some(now_ts());
                row.error = Some(format!(
                    "a definitive verdict's run log could not be persisted and no redelivery \
                     remains; settled failed by explicit operator resolution ({operator}): \
                     {rationale}"
                ));
                row.reason = Some(format!(
                    "failed; evidence unavailable, resolved by operator: {rationale}"
                ));
            }
            Some(())
        })
        .await
        .map_err(map_store)?;
    let Some(outcome) = outcome else {
        let current = state
            .store
            .get_gate_job(&job_id)
            .await
            .map_err(map_store)?
            .ok_or(HubError::NotFound)?;
        if current.evidence_uncertain.is_none() {
            return Err(HubError::NotFound);
        }
        return Err(HubError::Conflict(
            "the live uncertain condition is no longer the attempt this resolution named; refresh \
             and resolve the current attempt"
                .to_string(),
        ));
    };
    if requeue {
        journal(&state, &device.id, "gate.base-moved-requeue", &outcome).await;
        schedule_once(&state);
    } else {
        journal(&state, &device.id, "gate.failed", &outcome).await;
        if was_landing {
            drop_job_refs(&state, &outcome).await;
        }
    }
    Ok(Json(json!(outcome)))
}

async fn resolve_job(
    state: &AppState,
    headers: &HeaderMap,
    project: &str,
    job_id: &str,
) -> Result<GateJob, HubError> {
    let device = caller(state, headers).await?;
    let scope = caller_project_scope(state, &device).await?;
    if !scope.allows_project(project) {
        return Err(HubError::Forbidden);
    }
    let job = state
        .store
        .get_gate_job(job_id)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if job.project_id.as_id().as_str() != project {
        return Err(HubError::NotFound);
    }
    Ok(job)
}

/// `GET /v1/gate/logs/{id}` — bounded failure evidence for one gate run.
/// Accepts either the `gjb_…` job id (resolves to its latest log) or the
/// `obj_…` log object id; visible to any caller with project scope.
async fn get_gate_log(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, HubError> {
    if id.is_empty() || !(id.starts_with("gjb_") || id.starts_with("obj_")) {
        return Err(HubError::BadRequest(
            "expected a gjb_ job id or obj_ log object id".into(),
        ));
    }
    let device = caller(&state, &headers).await?;
    let scope = caller_project_scope(&state, &device).await?;
    let object = state
        .store
        .get_gate_log(&id)
        .await
        .map_err(map_store)?
        .ok_or(HubError::NotFound)?;
    if crate::config::now_rfc3339().as_str() >= object.expires_at.as_str() {
        return Err(HubError::NotFound);
    }
    if !scope.allows_project(&object.project_id) {
        return Err(HubError::Forbidden);
    }
    let log: Value = serde_json::from_slice(&object.bytes)
        .map_err(|error| HubError::Internal(error.to_string()))?;
    Ok(Json(json!({
        "objectId": object.object_id,
        "jobId": object.job_id,
        "projectId": object.project_id,
        "expiresAt": object.expires_at,
        "log": log,
    })))
}

fn filter_jobs(
    jobs: &mut Vec<GateJob>,
    state: Option<&str>,
    branch: Option<&str>,
    limit: Option<u32>,
) {
    if let Some(wanted) = state {
        jobs.retain(|job| job.state.as_str() == wanted);
    }
    if let Some(branch) = branch {
        jobs.retain(|job| job.branch == branch);
    }
    if let Some(limit) = limit {
        jobs.truncate(limit as usize);
    }
}

fn validate_branch(branch: &str) -> Result<(), HubError> {
    if branch.is_empty() || branch.starts_with('-') {
        return Err(HubError::BadRequest("invalid branch name".into()));
    }
    if branch == "main" || branch.starts_with("refs/heads/main") {
        return Err(HubError::BadRequest(
            "source branch must differ from main".into(),
        ));
    }
    Ok(())
}

// ── scheduler ──────────────────────────────────────────────────────────────

/// One scheduling pass: dispatch queued jobs respecting FIFO, one job per
/// lane, and one land at a time per project.
pub(crate) async fn tick(state: &AppState) {
    if let Err(error) = schedule(state).await {
        tracing::warn!(%error, "gate queue schedule tick failed");
    }
    // Finish jobs whose cancel grace has expired without a Node answer.
    finish_expired_cancels(state).await;
    // Pinned-ref retention; self-throttled, so this is cheap on every tick.
    sweep_expired_refs(state).await;
}

/// Finish any `canceling` job whose cancel grace has elapsed. A Node that never
/// answers `gate.cancel` must not decide the outcome: after the bounded grace
/// the Hub finishes the job `canceled`, keeping its evidence, and a later run
/// reply for that (now terminal) job is ignored rather than applied.
async fn finish_expired_cancels(state: &AppState) {
    let Ok(jobs) = state.store.list_gate_jobs(None).await else {
        return;
    };
    let grace_ms = state.config.gate_cancel_grace_ms;
    let now = now();
    for job in jobs {
        if job.state != GateJobState::Canceling {
            continue;
        }
        // A home host is still pushing main for this job: HOME_LAND_TIMEOUT is
        // far longer than the cancel grace, and land_from_home writes the
        // honest terminal state (landed if it pushed, canceled otherwise) and
        // clears this flag. Finalizing here would both race that write and drop
        // a pinned merge the home host is still fetching.
        if job.home_land_in_flight {
            continue;
        }
        // r23 R9: a Canceling row parked with an unpersisted definitive
        // verdict is held for an idempotent redelivery or explicit operator
        // resolution; the time-based cancel grace must not finish it and
        // discard the condition.
        if job.evidence_uncertain.is_some() {
            continue;
        }
        let Some(requested) = &job.cancel_requested_at else {
            continue;
        };
        let elapsed_ms = age_ms(&now, requested);
        if elapsed_ms.is_none_or(|age| age < grace_ms) {
            continue;
        }
        let finished = state
            .store
            .mutate_gate_job(job.id.as_id().as_str(), |row, _conn| {
                if row.state != GateJobState::Canceling
                    || row.home_land_in_flight
                    || row.evidence_uncertain.is_some()
                {
                    return None;
                }
                row.state = GateJobState::Canceled;
                row.finished_at = Some(now_ts());
                row.error =
                    Some("canceled; the lane did not answer within the cancel grace".into());
                row.reason = Some("canceled; run did not stop in time".into());
                Some(())
            })
            .await;
        if let Ok(Some(job)) = finished {
            journal(state, &job.requested_by, "gate.canceled", &job).await;
            drop_job_refs(state, &job).await;
            schedule_once(state);
        }
    }
}

async fn schedule(state: &AppState) -> Result<(), HubError> {
    let jobs = state.store.list_gate_jobs(None).await.map_err(map_store)?;
    if !jobs.iter().any(|job| job.state == GateJobState::Queued) {
        return Ok(());
    }
    // Lane usage from in-flight jobs. Landing is in-flight too: the granted
    // push still owns the lane checkout, and a land still holds the project's
    // single land slot until it reaches a terminal state. Counting only
    // Running let the scheduler dispatch a second job onto the same checkout
    // and start a second land while the first push was unresolved.
    let mut busy_lanes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut land_active: std::collections::HashSet<String> = std::collections::HashSet::new();
    for job in &jobs {
        if matches!(job.state, GateJobState::Running | GateJobState::Landing) {
            if let Some(lane) = &job.lane_id {
                busy_lanes.insert(format!("{}:{lane}", job.project_id.as_id()));
            }
            if job.mode == GateMode::Land {
                land_active.insert(job.project_id.as_id().to_string());
            }
        }
    }
    // Group queued jobs per project in FIFO order; once an earlier queued job
    // cannot dispatch this tick, later ones wait too (queue order).
    let mut queued: std::collections::BTreeMap<String, Vec<&GateJob>> =
        std::collections::BTreeMap::new();
    for job in jobs.iter().filter(|job| job.state == GateJobState::Queued) {
        queued
            .entry(job.project_id.as_id().to_string())
            .or_default()
            .push(job);
    }
    for (project_id, mut pending) in queued {
        pending.sort_by(|a, b| {
            String::from(a.queued_at.clone()).cmp(&String::from(b.queued_at.clone()))
        });
        for job in pending {
            let project = match state
                .store
                .get_project(project_id.clone())
                .await
                .map_err(map_store)?
            {
                Some(project) => project,
                None => continue,
            };
            if project.gate.lanes.is_empty() {
                continue;
            }
            if job.mode == GateMode::Land && land_active.contains(&project_id) {
                break;
            }
            let Some(lane) = pick_lane(
                &project.gate.lanes,
                job.lane_id.as_deref(),
                &project_id,
                &busy_lanes,
                state,
            )
            .await
            else {
                // No free connected lane: preserve FIFO for this project.
                break;
            };
            busy_lanes.insert(format!("{project_id}:{}", lane.id));
            if job.mode == GateMode::Land {
                land_active.insert(project_id.clone());
            }
            dispatch(
                state,
                job,
                lane,
                project.default_base_branch.clone(),
                resolve_push_from(lane, &project),
                merge_step_timeouts(&project.gate, lane),
            )
            .await;
        }
    }
    Ok(())
}

async fn pick_lane<'a>(
    lanes: &'a [remuda_protocol::ProjectGateLane],
    explicit: Option<&str>,
    project_id: &str,
    busy: &std::collections::HashSet<String>,
    state: &AppState,
) -> Option<&'a remuda_protocol::ProjectGateLane> {
    for lane in lanes {
        if let Some(wanted) = explicit
            && wanted != lane.id
        {
            continue;
        }
        if busy.contains(&format!("{project_id}:{}", lane.id)) {
            continue;
        }
        // Only dispatch to a Node that is actually connected.
        if state
            .nodes
            .kind_of(lane.host_id.as_id().as_str())
            .await
            .is_none()
        {
            continue;
        }
        return Some(lane);
    }
    None
}

/// Where a land job's push happens for this lane.
///
/// Explicit lane config wins. Absent it the default is `home` whenever the
/// lane cannot be shown to hold a push credential for the project remote —
/// the safe direction, because a lane that cannot push produces a passing but
/// unlandable verify (the D-034 hole this closes). A lane declares itself
/// push-capable by setting `pushFrom: lane`.
///
/// The credential probe is deliberately a lane-config assertion rather than a
/// live check: the Node reports no credential inventory today, and inventing
/// one here would guess at the operator's ssh/askpass setup. When a Node does
/// report it, this is the single place that changes.
fn resolve_push_from(
    lane: &remuda_protocol::ProjectGateLane,
    project: &remuda_protocol::Project,
) -> remuda_protocol::GatePushFrom {
    if let Some(explicit) = lane.push_from {
        return explicit;
    }
    // No project remote to push to: nothing for a home host to do either, so
    // keep the historical lane behaviour.
    if project.repo_remote.is_none() {
        return remuda_protocol::GatePushFrom::Lane;
    }
    // A lane that named a fetchRemote is configured for the home-host handoff.
    if lane.fetch_remote.is_some() {
        return remuda_protocol::GatePushFrom::Home;
    }
    remuda_protocol::GatePushFrom::Lane
}

/// Per-step wall-clock budgets for a run: project defaults with the lane's own
/// entries layered on top (lane wins per key). `0` seconds is preserved so the
/// runner reads it as "no cap" for that step.
fn merge_step_timeouts(
    gate: &remuda_protocol::ProjectGate,
    lane: &remuda_protocol::ProjectGateLane,
) -> std::collections::BTreeMap<String, u64> {
    let mut merged = gate.timeouts.clone();
    for (step, secs) in &lane.timeouts {
        merged.insert(step.clone(), *secs);
    }
    merged
}

async fn dispatch(
    state: &AppState,
    job: &GateJob,
    lane: &remuda_protocol::ProjectGateLane,
    base_branch: String,
    push_from: remuda_protocol::GatePushFrom,
    timeouts: std::collections::BTreeMap<String, u64>,
) {
    let claim_lane = lane.id.clone();
    let claim_host = lane.host_id.clone();
    let claimed = state
        .store
        .claim_gate_job(job.id.as_id().as_str(), claim_lane, claim_host)
        .await;
    let job = match claimed {
        Ok(GateJobClaim::Claimed(job)) => job,
        Ok(GateJobClaim::FencedCanceled(job)) => {
            // The claim writer settled the row canceled(fenced); journal the
            // outcome and send no gate.run.
            journal(state, &job.requested_by, "gate.canceled", &job).await;
            return;
        }
        Ok(GateJobClaim::NotReady) => return,
        Err(error) => {
            // Do not claim on an unrelated store failure: the next tick
            // retries the queued row.
            tracing::warn!(%error, job_id = job.id.as_id().as_str(), "gate claim failed");
            return;
        }
    };
    let params = GateRunParams {
        job_id: job.id.as_id().to_string(),
        lane_id: lane.id.clone(),
        repo_path: lane.repo_path.clone(),
        target_dir: lane.target_dir.clone(),
        branch: job.branch.clone(),
        base_branch,
        mode: job.mode.as_str().to_owned(),
        web: job.web.as_str().to_owned(),
        env: lane.env.clone(),
        lock_path: lane.lock_path.clone(),
        pw_endpoint: lane.pw_endpoint.clone(),
        toolchain_path: lane.toolchain_path.clone(),
        ports: lane.ports.clone(),
        timeouts,
        gate_timeout_secs: 0,
        push: true,
        push_from,
        keep_logs: job.keep_logs,
        binary: None,
        // §7.5: gate.run carries the admission record's initiator; the jobId
        // is the opId. Node-side refusal arrives with ma-fence.
        initiator: job.initiator.clone(),
        // Dispatch generation: the attempt ordinal the claim writer stamped.
        // The Node refuses this exact dispatch if a gate.cancel for the same
        // generation is acked before it registers, however delayed the frame
        // is (elapsed time never authorizes the old dispatch).
        generation: job.attempts,
    };
    journal(state, &job.requested_by, "gate.running", &job).await;
    let state = state.clone();
    let job_id = job.id.as_id().to_string();
    // r11 item 4: the generation THIS dispatch claimed; its error/timeout must
    // only fail that attempt, never a newer Running attempt after a requeue.
    let dispatch_generation = params.generation;
    let host_id = lane.host_id.clone();
    // r12 item 2: bind THIS dispatch to the authenticated carrier session the
    // gate.run is about to ride (the Hub node-link generation minted at that
    // socket's hello). Recorded before the frame is sent, so the Node's
    // pre-run authorize and the verify→land grant can require the SAME
    // session; a reconnect that replaces the session makes the old run's
    // grant fail closed.
    let dispatch_session = state.nodes.generation_of(host_id.as_id().as_str()).await;
    if let Some(session) = dispatch_session {
        let _ = state
            .store
            .set_gate_dispatch_session(job_id.as_str(), session)
            .await;
    }
    tokio::spawn(async move {
        let reply = state
            .nodes
            .call(
                host_id.as_id().as_str(),
                "gate.run",
                serde_json::to_value(params).unwrap_or(Value::Null),
                GATE_RUN_TIMEOUT,
            )
            .await;
        match reply {
            Ok(Some(frame)) => {
                // The finished event and the reply carry the same verdict;
                // apply_result is idempotent on the running state.
                if let Some(result) = frame.get("result") {
                    if let Ok(result) = serde_json::from_value::<GateRunResult>(result.clone()) {
                        // r13 item 4: the RPC reply settles only the dispatch's
                        // own granted session.
                        apply_result(&state, &job_id, result, dispatch_session).await;
                    }
                } else if let Some(error) = frame.get("error") {
                    fail_rpc(&state, &job_id, dispatch_generation, error).await;
                }
            }
            Ok(None) => {
                fail_rpc(
                    &state,
                    &job_id,
                    dispatch_generation,
                    &json!({"message": "lane host is not connected"}),
                )
                .await;
            }
            Err(error) => {
                fail_rpc(
                    &state,
                    &job_id,
                    dispatch_generation,
                    &json!({"message": error.to_string()}),
                )
                .await
            }
        }
        schedule_once(&state);
    });
}

async fn fail_rpc(state: &AppState, job_id: &str, expected_generation: u32, error: &Value) {
    fail_rpc_inner(state, job_id, expected_generation, error).await
}

async fn fail_rpc_inner(state: &AppState, job_id: &str, expected_generation: u32, error: &Value) {
    // r13 item 2: the grant was taken and the gate.run reply/link was lost. The
    // row stays Landing and keeps occupancy, but the loss must not strand it:
    // kick the bounded read-only reconciliation for its lane host now (a
    // still-connected carrier is probed immediately; a dropped one is handled
    // when it reconnects), in addition to the periodic reconciler.
    if let Ok(Some(job)) = state.store.get_gate_job(job_id).await
        && job.state == GateJobState::Landing
        && job.attempts == expected_generation
    {
        tracing::info!(
            expected_generation,
            error = %message_or_default(error),
            "gate.run RPC lost during Landing; scheduling bounded reconciliation"
        );
        if let Some(host) = landing_lane_host(state, &job).await {
            schedule_landing_reconcile(state, Some(host.as_id().to_string()));
        }
        return;
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("gate.run failed")
        .to_owned();
    let updated = state
        .store
        .mutate_gate_job(job_id, move |row, _conn| {
            // Landing is an in-flight state: the grant already authorized the
            // push and the lane child may be mid-push. A gate.run RPC error or
            // timeout here cannot say the push failed (it may still complete),
            // so the row stays Landing and is settled by the land verdict / a
            // finished event / restart reconcile — never failed optimistically.
            if row.state == GateJobState::Landing {
                tracing::info!(
                    expected_generation,
                    "gate.run RPC failure during Landing; leaving the push to settle"
                );
                return None;
            }
            if row.state != GateJobState::Running && row.state != GateJobState::Canceling {
                return None;
            }
            // r11 item 4: an RPC error/timeout belongs to the dispatch that
            // made the call; after a base-moved requeue the row is a newer
            // Running attempt and must not be failed.
            if row.attempts != expected_generation {
                tracing::info!(
                    expected_generation,
                    writer_attempt = row.attempts,
                    "dropping gate.run RPC failure for a non-current attempt"
                );
                return None;
            }
            // The lane's `gate.run` RPC stays outstanding across a `pushFrom:
            // home` handoff, so a lane disconnect or timeout can land here
            // while the home host is mid-push. Stand down: land_from_home owns
            // the terminal write and records the honest outcome.
            if row.home_land_in_flight {
                return None;
            }
            row.state = GateJobState::Failed;
            row.finished_at = Some(now_ts());
            row.reason = Some(message.lines().next().unwrap_or(&message).to_owned());
            row.error = Some(message.clone());
            Some(())
        })
        .await;
    if let Ok(Some(job)) = updated {
        journal(state, &job.requested_by, "gate.failed", &job).await;
    }
    // Test-only completion latch (no-op in release): lets a test wait for the
    // stale-RPC-failure decision to fully return instead of sleeping.
    #[cfg(any(test, feature = "test-faults"))]
    {
        let barrier = state.store.gate_apply_barrier();
        barrier
            .rpc_completed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        barrier.rpc_completed_notify.notify_waiters();
    }
}

/// Apply a `gate.event` Node notification (runs for both WSS and ssh-stdio
/// through `ws::handle_node_method`).
pub(crate) async fn on_event(
    state: &AppState,
    from_host: &str,
    source_session: Option<u64>,
    params: Value,
) -> Result<(), HubError> {
    let event: remuda_protocol::GateEventParams =
        serde_json::from_value(params).map_err(|error| HubError::BadRequest(error.to_string()))?;
    let job_id = event.job_id.clone();
    if let Some(job) = state.store.get_gate_job(&job_id).await.map_err(map_store)?
        && let Some(host) = &job.host_id
        && host.as_id().as_str() != from_host
    {
        // Only the lane host the job was dispatched to may drive its events.
        return Err(HubError::Forbidden);
    }
    use remuda_protocol::GateEventKind as Kind;
    let event_generation = event.generation;
    match event.kind {
        Kind::Phase { .. } | Kind::Log { .. } => {}
        Kind::Step { step } => {
            let _ = state
                .store
                .mutate_gate_job(&job_id, move |row, _conn| {
                    if row.state != GateJobState::Running && row.state != GateJobState::Canceling {
                        return None;
                    }
                    // r11 item 5: ignore step evidence from a stale attempt so
                    // attempt-1 steps cannot overwrite attempt-2 evidence.
                    if row.attempts != event_generation {
                        return None;
                    }
                    match row
                        .steps
                        .iter()
                        .position(|existing| existing.name == step.name)
                    {
                        Some(index) => row.steps[index] = step,
                        None => row.steps.push(step),
                    }
                    Some(())
                })
                .await;
        }
        Kind::Finished { result } => {
            // r13 item 4: carry the inbound socket's authenticated host/session
            // so a replacement carrier cannot finish-settle a granted Landing.
            apply_result(state, &job_id, *result, source_session).await
        }
    }
    // Test-only completion latch (no-op in release): the event's writer has
    // run, so a test can assert the post-decision row without a sleep.
    #[cfg(any(test, feature = "test-faults"))]
    {
        let barrier = state.store.gate_apply_barrier();
        barrier
            .event_completed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        barrier.event_completed_notify.notify_waiters();
    }
    Ok(())
}

/// Apply a gate run verdict (RPC reply or terminal event). Idempotent: only a
/// running/canceling job transitions. `pub(crate)` for the deterministic
/// pre-read-vs-writer race test in the initiator suite.
pub(crate) async fn apply_result(
    state: &AppState,
    job_id: &str,
    result: GateRunResult,
    source_session: Option<u64>,
) {
    #[cfg(any(test, feature = "test-faults"))]
    let barrier = state.store.gate_apply_barrier();
    // The terminal `finished` event and the `gate.run` reply carry the same
    // verdict; this read makes the second arrival a no-op before it would
    // replace the stored log object.
    let Some(existing) = state.store.get_gate_job(job_id).await.ok().flatten() else {
        #[cfg(any(test, feature = "test-faults"))]
        barrier.note_completed();
        return;
    };
    // GENERATION FENCE: a verdict belongs to the attempt that produced it.
    // After a base-moved requeue the OLD attempt's delayed finished event or
    // RPC reply (generation < the current row) must be dropped entirely —
    // accepting it would requeue/kill the newly running attempt, burn a retry
    // and cause a later lane-busy. Only the exact current attempt applies.
    if result.generation != existing.attempts {
        tracing::info!(
            job_id,
            verdict_generation = result.generation,
            current_attempt = existing.attempts,
            "dropping gate verdict for a non-current attempt"
        );
        #[cfg(any(test, feature = "test-faults"))]
        barrier.note_completed();
        return;
    }
    if !matches!(
        existing.state,
        GateJobState::Running | GateJobState::Canceling | GateJobState::Landing
    ) {
        #[cfg(any(test, feature = "test-faults"))]
        barrier.note_completed();
        return;
    }
    // A Landing verdict settles the GRANTED attempt. It is accepted only when
    // the row actually carries a grant marker (the session the grant was
    // fenced to): a verdict that finds Landing but no `landingSessionEpoch`
    // did not come through a real Running → Landing grant and is dropped.
    // Host binding is enforced by `on_event` for finished events; the gate.run
    // reply is structurally the dispatched host's call.
    if existing.state == GateJobState::Landing && existing.landing_session_epoch.is_none() {
        tracing::warn!(
            job_id,
            "dropping verdict for a Landing row with no granted session marker"
        );
        #[cfg(any(test, feature = "test-faults"))]
        barrier.note_completed();
        return;
    }
    // r15 item 3 (R2): for a Landing verdict, PIN the lane host's live carrier
    // session for the whole decision stretch — session/grant/current checks,
    // the evidence transaction, and the verdict writer — with the per-host
    // shared authority guard. A hello replacement takes the guard exclusively,
    // so it can only happen entirely before this stretch or entirely after the
    // commit: a takeover between the pre-check and the writer is impossible.
    let _host_decision = if existing.state == GateJobState::Landing {
        if let Some(host) = existing.host_id.as_ref() {
            Some(
                state
                    .nodes
                    .host_authority(host.as_id().as_str())
                    .await
                    .read_owned()
                    .await,
            )
        } else {
            None
        }
    } else {
        None
    };
    // r13 item 4 SESSION FENCE: a Landing can only be settled by the exact
    // authenticated carrier session the grant was bound to. A replacement
    // carrier B that received A's delayed Finished/reply (generation and host
    // still equal) rides a different session and must not settle A's granted
    // push. Read the Hub-only persisted grant session and require the verdict's
    // source session to equal it. A Landing row's dispatch_session is immutable
    // while it stays Landing, so the value captured here is still the authority
    // inside the writer (which re-checks state + generation and this session).
    let grant_session = state
        .store
        .gate_job_dispatch_session(job_id)
        .await
        .ok()
        .flatten();
    let grant_session_u64 = grant_session.and_then(|v| u64::try_from(v).ok());
    if existing.state == GateJobState::Landing
        && source_session.is_none_or(|s| Some(s) != grant_session_u64)
    {
        tracing::warn!(
            job_id,
            ?source_session,
            ?grant_session_u64,
            "dropping Landing verdict from a session that does not hold the grant"
        );
        #[cfg(any(test, feature = "test-faults"))]
        barrier.note_completed();
        return;
    }
    // r14 R2 — authority is CURRENT, not merely granted. A takeover hello
    // supersedes the granted socket while its Landing push is still running:
    // the superseded socket keeps the persisted session identity (and passes
    // the equality above), but it is no longer the host's live session. Its
    // late Finished/RPC verdict and its run-log evidence stand down here; the
    // row stays Landing and the remote reconciler establishes the outcome. The
    // host guard above keeps this check true through the commit (r15 item 3).
    if existing.state == GateJobState::Landing
        && let Some(session) = grant_session_u64
        && let Some(host) = existing.host_id.as_ref()
        && !state
            .nodes
            .is_current_session(host.as_id().as_str(), session)
            .await
    {
        tracing::warn!(
            job_id,
            granted_session = session,
            "dropping Landing verdict from the granted session after it was superseded"
        );
        #[cfg(any(test, feature = "test-faults"))]
        barrier.note_completed();
        return;
    }
    // r13 item 5: a granted land child that hit its budget reports UNCERTAIN —
    // its group is terminated on the Node, but the remote may already have taken
    // the ref. Never turn that into Failed/landed from the verdict: keep the
    // Landing occupancy and let the bounded remote probe establish the truth
    // (immediate forced pass if the lane is connected, else on reconnect).
    if existing.state == GateJobState::Landing && result.status == "uncertain" {
        tracing::info!(
            job_id,
            "land reported uncertain after its timeout; leaving Landing for remote reconciliation"
        );
        if let Some(host) = landing_lane_host(state, &existing).await {
            schedule_landing_reconcile(state, Some(host.as_id().to_string()));
        }
        #[cfg(any(test, feature = "test-faults"))]
        barrier.note_completed();
        return;
    }
    // Test-only pre-read barrier: park here (row confirmed live + generation
    // matched) so two real arrivals can be held simultaneously.
    #[cfg(any(test, feature = "test-faults"))]
    state.store.gate_apply_barrier().wait().await;
    // Persist the bounded evidence out-of-band first; the job row keeps only
    // the `obj_…` reference. A persistence failure must not lose the verdict.
    let mut log_object_id: Option<String> = None;
    // r22 R9: the digest of the bytes ACTUALLY persisted (an oversized log is
    // stored truncated, so this differs from the supplied log's digest). The
    // evidence predicate binds the settlement to these exact bytes.
    let mut persisted_digest: Option<String> = None;
    if let Some(run_log) = &result.run_log {
        // r11 item 3: re-read attempts right before persisting so a stale
        // attempt's evidence cannot overwrite the current attempt's log.
        let live_attempts = state
            .store
            .get_gate_job(job_id)
            .await
            .ok()
            .flatten()
            .map(|job| job.attempts);
        if live_attempts == Some(result.generation) {
            match persist_run_log(
                state,
                existing.project_id.as_id().as_str(),
                job_id,
                &existing.requested_by,
                run_log,
                LogAuth {
                    generation: result.generation,
                    session: source_session,
                    // r16 item 5: bind the evidence to the exact merge the
                    // verdict carries (the granted merge on a Landing row).
                    merge_sha: result.merge_sha.clone(),
                },
                existing.host_id.as_ref().map(|host| host.as_id().as_str()),
            )
            .await
            {
                // None: the in-transaction generation/session gate found the row
                // advanced (or a non-granted Landing session) before the write.
                Ok(Some((object_id, digest))) => {
                    log_object_id = Some(object_id);
                    persisted_digest = Some(digest);
                }
                Ok(None) => {
                    log_object_id = None;
                }
                Err(error) => tracing::warn!(%error, job_id, "could not persist gate log object"),
            }
        } else {
            tracing::info!(
                job_id,
                verdict_generation = result.generation,
                ?live_attempts,
                "skipping log persist for a non-current attempt"
            );
        }
    }
    // r18 item 7 TEST-ONLY seam: park AFTER evidence persist but BEFORE the
    // settlement mutate, so a concurrent writer can replace the linked log
    // object to deterministically exercise the verdict↔evidence digest bind.
    // r21 item 6: keyed by this arrival's OWN persisted object id so a test
    // can release the two handlers independently.
    #[cfg(any(test, feature = "test-faults"))]
    state
        .store
        .gate_after_persist_barrier()
        .wait_key(log_object_id.as_deref().unwrap_or(""))
        .await;
    // A `pushFrom: home` land verifies on the lane and is pushed by the home
    // host. The lane therefore reports `passed` for a land job; the job stays
    // `running` across the handoff so `remuda land` keeps waiting instead of
    // reading a verify as a completed land.
    //
    // A canceling job is authoritative: the verdict verified the merge, but the
    // operator asked to cancel, so we never advance main. The one exception is
    // a `pushFrom: lane` land whose Node already pushed and reports `landed` —
    // main moved, so we keep `landed` and record the cancel arrived too late
    // (honesty over tidiness). Everything else canceling ends `canceled`,
    // keeping mergeSha/mergeRef/steps, with no home-host handoff.
    // The state at the pre-read decides the (awaited) home-land plan; the
    // mutate closure below re-checks the state INSIDE its writer job, because
    // a cancel request can commit between this read and the writer.
    let is_canceling = existing.state == GateJobState::Canceling;
    let home_land = if !is_canceling && existing.mode == GateMode::Land && result.status == "passed"
    {
        home_land_plan(state, &existing, &result).await
    } else {
        None
    };
    let landing = home_land.clone();
    // Set by the writer when the row is found Canceling INSIDE the mutate
    // job (a cancel that committed after the pre-read above). The post-writer
    // side effects must treat that the same as a pre-read Canceling row.
    let fresh_canceling = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fresh_canceling_writer = fresh_canceling.clone();
    // Set by the writer when a Landing verdict carries a non-authoritative
    // status (r18 item 2): the Landing is kept and reconciliation is scheduled
    // after the writer stands down.
    let non_authoritative_landing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let non_authoritative_landing_writer = non_authoritative_landing.clone();
    // r23 R9 clarification: set when the writer records the SECOND deferral of
    // the same unpersisted verdict — both the Node's deliveries are then
    // consumed with no replay remaining, so apply_result surfaces the
    // persisted UNCERTAIN condition to the operator feed.
    let evidence_uncertain_surfaced =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let evidence_uncertain_surfaced_writer = evidence_uncertain_surfaced.clone();
    // r23 item 2: bound the row-bound free-form fields (steps, failedStep,
    // reason, error) before they are copied into the persisted job doc. The
    // supplied run_log is left intact — the evidence digest binds its exact
    // supplied bytes and persist_run_log bounds the stored object separately.
    let mut result = result;
    bound_gate_result_row_fields(&mut result);
    let outcome = state
        .store
        .mutate_gate_job(job_id, move |row, conn| {
            // The LIVE row is the authority. Running and Canceling are the
            // pre-grant states; Landing is the granted in-flight push whose
            // verdict (landed / failed / base-moved) settles the row below.
            // Other (terminal) rows never move again.
            if !matches!(
                row.state,
                GateJobState::Running | GateJobState::Canceling | GateJobState::Landing
            ) {
                return None;
            }
            // r11 item 3: the pre-read generation check can be stale after a
            // requeue. Re-check generation INSIDE the writer against the live
            // doc: an attempt-1 verdict must never mutate an attempt-2 row.
            if row.attempts != result.generation {
                tracing::info!(
                    verdict_generation = result.generation,
                    writer_attempt = row.attempts,
                    "dropping gate verdict in writer for a non-current attempt"
                );
                return None;
            }
            // r19 item 4 / r20 item 7 / r22 R9: bind EVERY definitive
            // settlement to THIS result's OWN evidence.
            //  * this arrival PERSISTED its run_log (`new_object_id` Some):
            //    the object must have exactly this log's sha256 — read inside
            //    this write transaction, so a same-grant result whose object
            //    was attached by an interleaved writer is observed and the
            //    verdict stands down;
            //  * this arrival SUPPLIED a run_log but did NOT persist one
            //    (`new_object_id` None — a transient insert failure):
            //    settlement is allowed ONLY when the row already references an
            //    object of the SAME digest (an identical redelivery whose bytes
            //    are already the bound evidence). Otherwise it DEFERS (returns
            //    false): the job stays in its pre-settlement state and the
            //    result is NOT acknowledged/consumed, so the EXISTING delivery
            //    path (Node redelivery, uncertain-landing reconciliation,
            //    operator resolution) supplies the verdict again. The Hub runs
            //    NO persistence-retry scheduler of its own (coordinator ruling
            //    R9): such a scheduler recursively multiplied — every deferred
            //    retry spawned another complete retry chain — and could never
            //    heal a permanent rejection. An OVERSIZED log is not transient:
            //    `persist_run_log` truncates it with an explicit marker and the
            //    digest of exactly the stored bytes, so it settles normally.
            //    No run_log leaves the check vacuous.
            let evidence_is_own =
                |new_object_id: Option<&String>, row_object_id: Option<&str>| -> bool {
                    let Some(run_log) = result.run_log.as_ref() else {
                        return true;
                    };
                    // r22 R9: when this arrival persisted an object, bind to the
                    // digest of EXACTLY the stored bytes (a truncated oversized log
                    // has a different digest than the supplied bytes); otherwise
                    // hash the supplied log for the content-addressed redelivery
                    // comparison against an existing reference.
                    let own_digest = persisted_digest.clone().unwrap_or_else(|| {
                        crate::config::sha256_hex(&serde_json::to_vec(run_log).unwrap_or_default())
                    });
                    let digest_of = |id: &str| -> Option<String> {
                        conn.query_row(
                            "SELECT digest FROM gate_log_objects WHERE id = ?1",
                            params![id],
                            |r| r.get(0),
                        )
                        .optional()
                        .unwrap_or(None)
                        .flatten()
                    };
                    if let Some(new_object_id) = new_object_id {
                        return digest_of(new_object_id) == Some(own_digest);
                    }
                    // r20 item 7 / r22 R9: no object from THIS arrival. An
                    // existing reference settles only when its bytes ARE this
                    // result's log (content-addressed identity); an absent or
                    // different-digest reference DEFERS the settlement — the
                    // unconsumed verdict is re-supplied by the existing delivery
                    // path, never by a Hub-side retry scheduler.
                    match row_object_id.and_then(digest_of) {
                        Some(existing_digest) if existing_digest == own_digest => true,
                        linked => {
                            tracing::info!(
                                ?linked,
                                own_digest,
                                "deferring definitive gate settlement: the supplied run log was \
                                 not persisted and the linked evidence is absent or another \
                                 result's log; the result stays unconsumed for redelivery"
                            );
                            false
                        }
                    }
                };
            // r23 R9 clarification (replaces the bare stand-down): the Node
            // retains NO copy of a delivered verdict and redelivers nothing,
            // so when this definitive result's supplied log cannot be
            // persisted the Hub must not simply return and consume the verdict
            // into a forever-Running row. Record an explicit, PERSISTED
            // uncertain condition on the row WITHOUT moving its state (Running
            // / Canceling / Landing all keep their lane or land-slot
            // occupancy). The first deferral stays quiet — the dual delivery
            // (finished event + gate.run reply) may still heal it; once a
            // SECOND delivery of the SAME (generation, supplied digest)
            // defers, the condition is surfaced for operator resolution. An
            // idempotent later redelivery that persists clears it; there is
            // still no Hub-side retry scheduler (R9).
            let record_evidence_deferral = |row: &mut GateJob| {
                let Some(run_log) = result.run_log.as_ref() else {
                    return;
                };
                let supplied_digest =
                    crate::config::sha256_hex(&serde_json::to_vec(run_log).unwrap_or_default());
                let now = now_ts();
                let deliveries_deferred = if row
                    .evidence_uncertain
                    .as_ref()
                    .is_some_and(|condition| {
                        condition.generation == result.generation
                            && condition.supplied_digest == supplied_digest
                    }) {
                    let condition = row.evidence_uncertain.as_mut().expect("just matched Some");
                    condition.deliveries_deferred =
                        condition.deliveries_deferred.saturating_add(1);
                    condition.updated_at = now;
                    condition.deliveries_deferred
                } else {
                    row.evidence_uncertain =
                        Some(remuda_protocol::GateEvidenceUncertain {
                            generation: result.generation,
                            status: result.status.clone(),
                            supplied_digest,
                            deliveries_deferred: 1,
                            first_at: now.clone(),
                            updated_at: now,
                            reason: format!(
                                "a definitive '{}' verdict supplied a run log that could not be \
                                 persisted (transient storage failure); the Node retains no copy, \
                                 so the job keeps its pre-settlement state and occupancy pending an \
                                 identical redelivery or an explicit operator resolution",
                                result.status
                            ),
                        });
                    1
                };
                if deliveries_deferred >= 2 {
                    evidence_uncertain_surfaced_writer
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            };
            // Landing: settle the GRANTED attempt's land verdict. Cancel is not
            // grantable once Landing (the grant writer only takes Running), so
            // there is no Canceling sub-case here. Idempotent on the dual
            // finished-event/RPC-reply: the first arrival moves the row to a
            // terminal state, after which the top guard stands the second down.
            if row.state == GateJobState::Landing {
                // r13 item 4: re-assert the session fence INSIDE the writer.
                // The granted session is immutable while the row is Landing, so
                // the grant_session captured at the pre-read still names the
                // only session allowed to settle it; a verdict from a
                // replacement carrier stands down even if state/generation match.
                if source_session.is_none_or(|s| Some(s) != grant_session_u64) {
                    tracing::info!(
                        ?source_session,
                        ?grant_session_u64,
                        "dropping Landing verdict in writer from a non-granted session"
                    );
                    return None;
                }
                // r15 item 3: re-assert the HOST inside the writer too — the
                // row must still be bound to the host the authenticated verdict
                // arrived on (the closure's captured pre-read host), so a
                // settle can never cross hosts even if the doc was swapped.
                let verdict_host = existing
                    .host_id
                    .as_ref()
                    .map(|host| host.as_id().as_str())
                    .unwrap_or("");
                if row
                    .host_id
                    .as_ref()
                    .is_none_or(|host| host.as_id().as_str() != verdict_host)
                {
                    tracing::info!(
                        ?verdict_host,
                        "dropping Landing verdict in writer: row host no longer matches"
                    );
                    return None;
                }
                // r16 item 5 / r17 item 2: bind the GRANTED MERGE inside the
                // writer for EVERY Landing transition that moves the row
                // (landed / failed / base-moved). A verdict settles or requeues
                // THIS Landing only when it names the exact merge the grant
                // recorded; a verdict for a different sha or one with no merge
                // identity stands down and leaves the row Landing. The
                // production Node preserves the granted merge on its CAS-loss
                // (base-moved) branch (the merge was pinned at verify and is
                // retained on the result), so requiring it there loses no real
                // verdict but blocks a mismatched frame from discarding the
                // granted Landing. uncertain is exempt because it never moves
                // the row.
                if result.status != "uncertain"
                    && let Some(granted_merge) = row.merge_sha.as_deref()
                    && result.merge_sha.as_deref().filter(|sha| !sha.is_empty())
                        != Some(granted_merge)
                {
                    tracing::info!(
                        ?granted_merge,
                        verdict_merge = ?result.merge_sha,
                        verdict_status = %result.status,
                        "dropping Landing verdict in writer: merge does not match the granted merge"
                    );
                    return None;
                }
                // r13 item 5: uncertain never moves the Landing; the remote
                // reconciler settles it.
                if result.status == "uncertain" {
                    return None;
                }
                // r18 item 2: only an AUTHORITATIVE Landing outcome may move a
                // granted Landing. The whitelist is exactly landed / base-moved;
                // anything else ("", "passed", "bogus", "failed", …) is an
                // unknown terminal state — keep occupancy and let reconciliation
                // establish the truth rather than fall through to Failed.
                if !matches!(result.status.as_str(), "landed" | "base-moved") {
                    tracing::warn!(
                        verdict_status = %result.status,
                        verdict_merge = ?result.merge_sha,
                        "Landing verdict carries a non-authoritative status; keeping the Landing \
                         for reconciliation (not Failed)"
                    );
                    non_authoritative_landing_writer
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    return None;
                }
                for step in &result.steps {
                    match row
                        .steps
                        .iter()
                        .position(|existing| existing.name == step.name)
                    {
                        Some(index) => row.steps[index] = step.clone(),
                        None => row.steps.push(step.clone()),
                    }
                }
                row.base_sha = result.base_sha.clone().or(row.base_sha.clone());
                row.head_sha = result.head_sha.clone().or(row.head_sha.clone());
                row.merge_sha = result.merge_sha.clone().or(row.merge_sha.clone());
                row.merge_ref = result.merge_ref.clone().or(row.merge_ref.clone());
                row.current_main_sha = result.current_main_sha.clone();
                row.failed_step = result.failed_step.clone();
                row.reason = result.reason.clone();
                if let Some(object_id) = &log_object_id {
                    row.log_object_id = Some(object_id.clone());
                }
                row.finished_at = Some(now_ts());
                match result.status.as_str() {
                    "landed" => {
                        // r18 item 7 / r19 item 4: the winning landed verdict
                        // is bound to ITS OWN evidence object (digest checked in
                        // this transaction); a mismatch keeps the row Landing.
                        if !evidence_is_own(log_object_id.as_ref(), row.log_object_id.as_deref()) {
                            // r23: persist the explicit UNCERTAIN condition, keep the
                            // pre-settlement state and its occupancy.
                            record_evidence_deferral(row);
                            return Some(());
                        }
                        row.evidence_uncertain = None;
                        row.state = GateJobState::Landed;
                        row.error = None;
                    }
                    "base-moved" if row.attempts < MAX_LAND_ATTEMPTS => {
                        // The CAS lost: nothing was pushed. Re-queue a fresh
                        // verify onto the new base under the same attempt cap.
                        row.evidence_uncertain = None;
                        row.state = GateJobState::Queued;
                        row.started_at = None;
                        row.host_id = None;
                        row.lane_id = None;
                        row.landing_session_epoch = None;
                        row.finished_at = None;
                        row.current_main_sha = result.current_main_sha.clone();
                        row.merge_ref = None;
                        row.merge_sha = None;
                        row.failed_step = None;
                        row.reason = None;
                        row.log_object_id = None;
                    }
                    other => {
                        // r19 item 4: the attempt-cap Failed settlement is a
                        // DEFINITIVE settlement too — bind it to this result's
                        // own evidence, so a same-grant base-moved result can
                        // never pin another arrival's log (r18 only checked the
                        // landed arm).
                        if !evidence_is_own(log_object_id.as_ref(), row.log_object_id.as_deref()) {
                            // r23: persist the explicit UNCERTAIN condition, keep the
                            // pre-settlement state and its occupancy.
                            record_evidence_deferral(row);
                            return Some(());
                        }
                        row.evidence_uncertain = None;
                        row.state = GateJobState::Failed;
                        let error = result
                            .error
                            .clone()
                            .unwrap_or_else(|| format!("land ended after grant: {other}"));
                        row.reason = Some(error.lines().next().unwrap_or(&error).to_owned());
                        row.error = Some(error);
                    }
                }
                return Some(());
            }
            // Re-read the live state inside the writer job: the pre-read
            // above can be stale by one cancel commit. Without this, a land
            // `base-moved` verdict arriving in the same window would rewrite
            // the Canceling row back to Queued — the job runs again and can
            // land although the caller already received 200 canceling.
            let canceling = row.state == GateJobState::Canceling;
            if canceling {
                fresh_canceling_writer.store(true, std::sync::atomic::Ordering::Relaxed);
                // The lane's `gate.run` RPC stays outstanding across a
                // `pushFrom: home` handoff. If a home push is in flight, stand
                // down: land_from_home owns the terminal write and records the
                // honest outcome (landed if it pushed, canceled otherwise).
                if row.home_land_in_flight {
                    return None;
                }
                // Record the terminal evidence, then resolve the cancel.
                for step in &result.steps {
                    match row
                        .steps
                        .iter()
                        .position(|existing| existing.name == step.name)
                    {
                        Some(index) => row.steps[index] = step.clone(),
                        None => row.steps.push(step.clone()),
                    }
                }
                row.base_sha = result.base_sha.clone().or(row.base_sha.clone());
                row.head_sha = result.head_sha.clone().or(row.head_sha.clone());
                row.merge_sha = result.merge_sha.clone().or(row.merge_sha.clone());
                row.merge_ref = result.merge_ref.clone().or(row.merge_ref.clone());
                row.current_main_sha = result.current_main_sha.clone();
                if let Some(object_id) = &log_object_id {
                    row.log_object_id = Some(object_id.clone());
                }
                row.finished_at = Some(now_ts());
                if result.status == "landed" {
                    // The lane already pushed before the cancel reached it;
                    // main moved. Keep the truth rather than claim a cancel —
                    // bound to this result's own evidence (r19 item 4).
                    if !evidence_is_own(log_object_id.as_ref(), row.log_object_id.as_deref()) {
                        // r23: persist the explicit UNCERTAIN condition, keep the
                        // pre-settlement state and its occupancy.
                        record_evidence_deferral(row);
                        return Some(());
                    }
                    row.evidence_uncertain = None;
                    row.state = GateJobState::Landed;
                    row.error = None;
                    row.reason = Some("cancel arrived after the lane had pushed main".into());
                } else {
                    // r22 item 2: this Canceled transition is result-driven,
                    // so the own-evidence predicate gates it exactly like the
                    // Passed/Failed settlements: a verdict that SUPPLIED a log
                    // which did not persist as its own digest must DEFER (the
                    // row stays Canceling and the result is re-supplied),
                    // never record "verified but not landed" — or a supplied
                    // "canceled" verdict — under missing or another result's
                    // bytes. A verdict with no run_log leaves the predicate
                    // vacuous.
                    if !evidence_is_own(log_object_id.as_ref(), row.log_object_id.as_deref()) {
                        // r23: persist the explicit UNCERTAIN condition, keep the
                        // pre-settlement state and its occupancy.
                        record_evidence_deferral(row);
                        return Some(());
                    }
                    row.evidence_uncertain = None;
                    row.state = GateJobState::Canceled;
                    if result.status == "passed" {
                        // The run verified green; only the land was skipped.
                        row.error = Some(result.error.clone().unwrap_or_else(|| {
                            "canceled after the run verified but was not landed".into()
                        }));
                        row.reason = Some("canceled; verified but not landed".into());
                    } else {
                        // The run itself ended canceled/failed: keep its own
                        // evidence rather than claim it verified.
                        row.failed_step = result.failed_step.clone().or(row.failed_step.clone());
                        row.error = result
                            .error
                            .clone()
                            .or_else(|| Some(format!("canceled; run ended {}", result.status)));
                        row.reason = result
                            .reason
                            .clone()
                            .or_else(|| Some(format!("canceled; run ended {}", result.status)));
                    }
                }
                return Some(());
            }
            if result.status == "canceled" {
                // r22 item 2: a direct supplied-log canceled verdict is a
                // definitive settlement — bind it to its OWN persisted digest
                // and defer (Running) when its supplied log did not persist,
                // exactly like every other supplied-log settlement.
                if !evidence_is_own(log_object_id.as_ref(), row.log_object_id.as_deref()) {
                    // r23: persist the explicit UNCERTAIN condition, keep the
                    // pre-settlement state and its occupancy.
                    record_evidence_deferral(row);
                    return Some(());
                }
                if let Some(object_id) = &log_object_id {
                    row.log_object_id = Some(object_id.clone());
                }
                row.evidence_uncertain = None;
                row.failed_step = result.failed_step.clone();
                row.state = GateJobState::Canceled;
                row.finished_at = Some(now_ts());
                row.error = result.error.clone();
                row.reason = result
                    .reason
                    .clone()
                    .or_else(|| Some("canceled".to_owned()));
                return Some(());
            }
            for step in &result.steps {
                match row
                    .steps
                    .iter()
                    .position(|existing| existing.name == step.name)
                {
                    Some(index) => row.steps[index] = step.clone(),
                    None => row.steps.push(step.clone()),
                }
            }
            if landing.is_some() && row.merge_ref.is_some() && row.merge_ref == result.merge_ref {
                // The terminal event and the RPC reply carry the same verdict.
                // The first arrival claimed the handoff by recording mergeRef;
                // this is the second, and must not start a second push.
                return None;
            }
            row.base_sha = result.base_sha.clone().or(row.base_sha.clone());
            row.head_sha = result.head_sha.clone().or(row.head_sha.clone());
            row.merge_sha = result.merge_sha.clone().or(row.merge_sha.clone());
            row.merge_ref = result.merge_ref.clone().or(row.merge_ref.clone());
            row.current_main_sha = result.current_main_sha.clone();
            row.failed_step = result.failed_step.clone();
            row.reason = result.reason.clone();
            if let Some(object_id) = &log_object_id {
                row.log_object_id = Some(object_id.clone());
            }
            if landing.is_some() {
                // Hold `running` for the home-host push; the land task below
                // writes the terminal state.
                return Some(());
            }
            if result.status == "passed" && row.mode == GateMode::Verify {
                // r21 item 4 / r22 R9: Passed is a DEFINITIVE settlement too
                // (a Node green run supplies its kept log with keepLogs).
                // Require this verdict's OWN persisted digest before the Passed
                // transition; a failed/absent insert defers (Running) with the
                // result unconsumed, so the existing delivery path re-supplies
                // it — there is no Hub-side retry (R9).
                if !evidence_is_own(log_object_id.as_ref(), row.log_object_id.as_deref()) {
                    // r23: persist the explicit UNCERTAIN condition, keep the
                    // pre-settlement state and its occupancy.
                    record_evidence_deferral(row);
                    return Some(());
                }
                row.evidence_uncertain = None;
                row.state = GateJobState::Passed;
                row.finished_at = Some(now_ts());
            } else if result.status == "landed" {
                // r19 item 4: a definitive landed settlement binds its own
                // evidence digest.
                if !evidence_is_own(log_object_id.as_ref(), row.log_object_id.as_deref()) {
                    // r23: persist the explicit UNCERTAIN condition, keep the
                    // pre-settlement state and its occupancy.
                    record_evidence_deferral(row);
                    return Some(());
                }
                row.evidence_uncertain = None;
                row.state = GateJobState::Landed;
                row.finished_at = Some(now_ts());
                row.error = None;
            } else if result.status == "base-moved"
                && row.mode == GateMode::Land
                && row.attempts < MAX_LAND_ATTEMPTS
            {
                row.evidence_uncertain = None;
                row.state = GateJobState::Queued;
                row.started_at = None;
                row.host_id = None;
                row.lane_id = None;
                // The next attempt produces fresh evidence.
                row.failed_step = None;
                row.reason = None;
                row.log_object_id = None;
            } else {
                // r19 item 4: a definitive failure (incl. an attempt-cap
                // base-moved Failed reached through this arm) binds its own
                // evidence digest.
                if !evidence_is_own(log_object_id.as_ref(), row.log_object_id.as_deref()) {
                    // r23: persist the explicit UNCERTAIN condition, keep the
                    // pre-settlement state and its occupancy.
                    record_evidence_deferral(row);
                    return Some(());
                }
                row.evidence_uncertain = None;
                row.state = GateJobState::Failed;
                row.finished_at = Some(now_ts());
                row.error = result
                    .error
                    .clone()
                    .or_else(|| Some(format!("gate ended: {}", result.status)));
                if row.reason.is_none()
                    && let Some(error) = &row.error
                {
                    row.reason = Some(error.lines().next().unwrap_or(error).to_owned());
                }
            }
            Some(())
        })
        .await;
    let Ok(Some(job)) = outcome else {
        // Writer stood down (terminal guard / duplicate / generation mismatch /
        // non-authoritative Landing). A non-authoritative Landing verdict keeps
        // occupancy and gets a reconciliation; either way count completion.
        if non_authoritative_landing.load(std::sync::atomic::Ordering::Relaxed) {
            let host = state
                .store
                .get_gate_job(job_id)
                .await
                .ok()
                .flatten()
                .and_then(|job| job.host_id.map(|host| host.as_id().to_string()));
            schedule_landing_reconcile(state, host);
        }
        // r22 R9: a true stand-down (terminal guard / duplicate / generation
        // mismatch / non-authoritative Landing) changes nothing. An evidence
        // deferral is handled on the Some(()) path below: it persists an
        // explicit uncertain condition rather than consuming the verdict
        // silently.
        #[cfg(any(test, feature = "test-faults"))]
        barrier.note_completed();
        return;
    };
    // r23 R9 clarification: the writer recorded the SECOND deferral of the
    // same verdict (finished event AND gate.run reply both failed to persist
    // the supplied log). No delivery remains that could re-supply it, so
    // surface the persisted uncertain condition on the operator feed. The
    // job's state and lane/land occupancy are deliberately untouched.
    if evidence_uncertain_surfaced.load(std::sync::atomic::Ordering::Relaxed) {
        journal(state, &job.requested_by, "gate.evidence-uncertain", &job).await;
        #[cfg(any(test, feature = "test-faults"))]
        barrier.note_completed();
        return;
    }
    // A claimed handoff: verified on the lane, now push from the home host.
    // The closure keeps the row `running` ONLY while the handoff still
    // stands: a cancel the writer found after the pre-check ended the row
    // (Canceled/Landed), so a plan from the stale pre-read must not fire.
    if let Some(plan) = home_land
        && job.state == GateJobState::Running
    {
        journal(state, &job.requested_by, "gate.verified", &job).await;
        let state = state.clone();
        let job_id = job.id.as_id().to_string();
        tokio::spawn(async move {
            land_from_home(&state, &job_id, plan).await;
            schedule_once(&state);
        });
        return;
    }
    let action = match job.state {
        GateJobState::Passed => Some("gate.passed"),
        GateJobState::Landed => Some("gate.landed"),
        GateJobState::Failed => Some("gate.failed"),
        GateJobState::Canceled => Some("gate.canceled"),
        GateJobState::Queued => Some("gate.base-moved-requeue"),
        _ => None,
    };
    if let Some(action) = action {
        journal(state, &job.requested_by, action, &job).await;
    }
    // A canceled job's pinned merge will never be landed; a lane-side land
    // already consumed its own pin. Either way the refs go now rather than
    // waiting for retention. The exception is a job canceled after it verified
    // (the row was Canceling at the pre-read OR the writer found it
    // Canceling): it keeps its pinned merge so an operator can still land it
    // by hand, and retention drops the ref later like any unlanded verify.
    let canceling = is_canceling || fresh_canceling.load(std::sync::atomic::Ordering::Relaxed);
    if matches!(job.state, GateJobState::Landed)
        || (job.state == GateJobState::Canceled && !canceling)
    {
        drop_job_refs(state, &job).await;
    }
    schedule_once(state);
    if job.state == GateJobState::Landed {
        run_then_hook(state, &job).await;
    }
    // r11 item 7: let tests that park arrivals know the full apply path
    // (persist + writer + side effects) has returned.
    #[cfg(any(test, feature = "test-faults"))]
    barrier.note_completed();
}

/// Serialize and store the bounded run log as an immutable,
/// content-addressed `obj_…` gate log object (r19 item 4): one object per
/// verdict, keyed by the job and the evidence sha256, so same-job arrivals
/// never overwrite one another and a settlement binds only the object its own
/// result persisted.
/// What a bounded run log object is persisted for: the attempt generation and
/// the authenticated carrier session the verdict rode (r13 item 4). Evidence
/// for a Landing may only be written by the granted session.
#[derive(Clone)]
pub(crate) struct LogAuth {
    pub(crate) generation: u32,
    pub(crate) session: Option<u64>,
    /// r16 item 5: the exact GRANTED merge sha this evidence belongs to. A
    /// Landing evidence write is rejected unless it names the row's granted
    /// merge — evidence for a different outcome is dropped before persistence.
    pub(crate) merge_sha: Option<String>,
}

async fn persist_run_log(
    state: &AppState,
    project_id: &str,
    job_id: &str,
    created_by: &str,
    run_log: &remuda_protocol::GateRunLog,
    auth: LogAuth,
    host: Option<&str>,
) -> Result<Option<(String, String)>, HubError> {
    // r22 item 1 / R9: an OVERSIZED log is not a transient insert failure —
    // a redelivery carries the identical oversized bytes forever, so
    // deferral could never heal. Truncate with an explicit marker and settle
    // normally against the digest of EXACTLY the bytes stored.
    let (bytes, _truncated) = persisted_run_log_bytes(run_log)?;
    let digest = crate::config::sha256_hex(&bytes);
    state
        .store
        .insert_gate_log(
            project_id,
            job_id,
            bytes,
            digest.clone(),
            created_by,
            auth,
            host,
        )
        .await
        .map(|object| object.map(|object_id| (object_id, digest)))
        .map_err(HubError::Store)
}

/// Marker prepended to the tail of an evidence object that was truncated to
/// the [`GATE_LOG_MAX_BYTES`] cap.
const GATE_LOG_TRUNCATION_MARKER: &str = "... run log truncated: it exceeded \
the 512 KiB evidence cap, so output beyond the stored tail was discarded \
before persistence ...";

// r23 item 2: per-field byte bounds for EVERY variable-length serialized
// field of a supplied GateRunLog. The Node bounds these at capture, but a
// log can arrive from any carrier version, so the Hub persistence path
// enforces them too before the final byte cap. All are far larger than a
// real value but small enough that their sum (plus the bounded tail/summary)
// cannot approach GATE_LOG_MAX_BYTES.
const GATE_LOG_STEP_MAX_BYTES: usize = 128;
const GATE_LOG_KIND_MAX_BYTES: usize = 32;
const GATE_LOG_HEADLINE_MAX_BYTES: usize = 8 * 1024;
const GATE_LOG_LINE_MAX_BYTES: usize = 8 * 1024;
// r23 item 2: bounds for the verdict's free-form fields that land in the JOB
// ROW (steps, failedStep, reason, error). The Node clips these at capture;
// the Hub enforces the same bounds before persisting any supplied verdict.
const GATE_JOB_REASON_MAX_BYTES: usize = 8 * 1024;
const GATE_JOB_ERROR_MAX_BYTES: usize = 16 * 1024;

/// Clip a free-form evidence field on a UTF-8 boundary, marking the cut.
/// Input already within the cap is returned verbatim.
fn clip_evidence_field(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut cut = max_bytes.saturating_sub("…".len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = text[..cut].to_owned();
    out.push('…');
    out
}

/// r23 item 2: bound every variable-length field of a supplied verdict that
/// is persisted into the gate JOB ROW (not the separate evidence object,
/// which [`persisted_run_log_bytes`] bounds). The `run_log` is deliberately
/// left untouched: its supplied bytes are what the evidence digest binds, and
/// persistence truncates that object independently.
fn bound_gate_result_row_fields(result: &mut GateRunResult) {
    for step in &mut result.steps {
        step.name = clip_evidence_field(&step.name, GATE_LOG_STEP_MAX_BYTES);
        step.error = step
            .error
            .take()
            .map(|text| clip_evidence_field(&text, GATE_JOB_REASON_MAX_BYTES));
        step.reason = step
            .reason
            .take()
            .map(|text| clip_evidence_field(&text, GATE_JOB_REASON_MAX_BYTES));
    }
    result.failed_step = result
        .failed_step
        .take()
        .map(|text| clip_evidence_field(&text, GATE_LOG_STEP_MAX_BYTES));
    result.reason = result
        .reason
        .take()
        .map(|text| clip_evidence_field(&text, GATE_JOB_REASON_MAX_BYTES));
    result.error = result
        .error
        .take()
        .map(|text| clip_evidence_field(&text, GATE_JOB_ERROR_MAX_BYTES));
}

/// Serialize a supplied run log for persistence. Logs within the cap are
/// stored verbatim; an oversized log is bounded on EVERY variable-length
/// field (step, kind, headline, each summary/tail line), then truncated
/// (tail lines first, then summary, then the headline) with
/// [`GATE_LOG_TRUNCATION_MARKER`] kept as the first tail line and the
/// `truncated` flag set, and the digest is computed by the caller over
/// EXACTLY the returned bytes. The 512 KiB cap is enforced in RUNTIME code
/// (not a debug assert): a value that still cannot be bounded is an error,
/// so an oversized object is never persisted in any build.
fn persisted_run_log_bytes(
    run_log: &remuda_protocol::GateRunLog,
) -> Result<(Vec<u8>, bool), HubError> {
    let encode = |log: &remuda_protocol::GateRunLog| -> Result<Vec<u8>, HubError> {
        serde_json::to_vec(log).map_err(|error| HubError::Internal(error.to_string()))
    };
    let bytes = encode(run_log)?;
    if bytes.len() <= GATE_LOG_MAX_BYTES {
        return Ok((bytes, false));
    }
    let mut bounded = run_log.clone();
    bounded.truncated = true;
    bounded.captured_lines = run_log.captured_lines.max(run_log.tail.len());
    // Bound the scalar fields and every individual line FIRST, so an enormous
    // step/kind/headline or a single huge line cannot survive the
    // line-count halving passes below (the r22 truncator only halved tail and
    // summary and trimmed the headline, leaving step/kind unbounded).
    bounded.step = clip_evidence_field(&bounded.step, GATE_LOG_STEP_MAX_BYTES);
    bounded.kind = clip_evidence_field(&bounded.kind, GATE_LOG_KIND_MAX_BYTES);
    bounded.headline = clip_evidence_field(&bounded.headline, GATE_LOG_HEADLINE_MAX_BYTES);
    bounded.summary = bounded
        .summary
        .iter()
        .map(|line| clip_evidence_field(line, GATE_LOG_LINE_MAX_BYTES))
        .collect();
    bounded.tail = bounded
        .tail
        .iter()
        .map(|line| clip_evidence_field(line, GATE_LOG_LINE_MAX_BYTES))
        .collect();
    // The marker is inserted AFTER per-line clipping so it is never itself
    // cut, and it stays index 0 so the halving truncates below always keep it.
    bounded
        .tail
        .insert(0, GATE_LOG_TRUNCATION_MARKER.to_owned());
    let mut bytes = encode(&bounded)?;
    // Halving passes keep the iteration count O(log line count) even for a
    // log made of hundreds of thousands of tiny lines.
    while bytes.len() > GATE_LOG_MAX_BYTES && bounded.tail.len() > 1 {
        let keep = (bounded.tail.len() / 2).max(1);
        bounded.tail.truncate(keep);
        bytes = encode(&bounded)?;
    }
    while bytes.len() > GATE_LOG_MAX_BYTES && !bounded.summary.is_empty() {
        let keep = bounded.summary.len() / 2;
        bounded.summary.truncate(keep);
        bytes = encode(&bounded)?;
    }
    if bytes.len() > GATE_LOG_MAX_BYTES {
        // Keep only the marker and trim the headline on a UTF-8 boundary to
        // leave room for the JSON frame.
        bounded.tail.truncate(1);
        bounded.summary.clear();
        bytes = encode(&bounded)?;
        if bytes.len() > GATE_LOG_MAX_BYTES {
            let framing = bytes.len().saturating_sub(bounded.headline.len());
            let budget = GATE_LOG_MAX_BYTES.saturating_sub(framing + 64);
            bounded.headline = clip_evidence_field(&bounded.headline, budget);
            bytes = encode(&bounded)?;
        }
    }
    if bytes.len() > GATE_LOG_MAX_BYTES {
        // Last-resort guarantee: marker + minimal scalars only.
        bounded.headline.clear();
        bounded.step = clip_evidence_field(&bounded.step, 16);
        bounded.kind = clip_evidence_field(&bounded.kind, 8);
        bytes = encode(&bounded)?;
    }
    // Runtime enforcement in EVERY build: the persisted object cannot exceed
    // the cap. Reaching here is a fail-closed internal error rather than an
    // oversized insert.
    if bytes.len() > GATE_LOG_MAX_BYTES {
        return Err(HubError::Internal(format!(
            "gate run log of {} bytes could not be bounded under the {} byte evidence cap",
            bytes.len(),
            GATE_LOG_MAX_BYTES
        )));
    }
    Ok((bytes, true))
}

/// What a home-host land needs, resolved once while the verdict is applied.
#[derive(Clone)]
struct HomeLandPlan {
    home_host: remuda_protocol::HostId,
    params: remuda_protocol::GateLandParams,
    lane_host: remuda_protocol::HostId,
    lane_repo: String,
}

/// Decide whether a passing land job hands off to the home host, and gather
/// everything the push needs.
///
/// Returns `None` when the lane pushes for itself, when the pieces are missing
/// (no pinned ref, no home host, no `fetchRemote`), or when the home host is
/// the lane host — in which case the lane's own land path already applies.
async fn home_land_plan(
    state: &AppState,
    job: &GateJob,
    result: &GateRunResult,
) -> Option<HomeLandPlan> {
    let project = state
        .store
        .get_project(job.project_id.as_id().to_string())
        .await
        .ok()
        .flatten()?;
    let lane = project
        .gate
        .lanes
        .iter()
        .find(|lane| Some(&lane.id) == job.lane_id.as_ref())?;
    if resolve_push_from(lane, &project) != remuda_protocol::GatePushFrom::Home {
        return None;
    }
    let home_host = project.home_host.clone()?;
    let merge_ref = result.merge_ref.clone().or_else(|| job.merge_ref.clone())?;
    let merge_sha = result.merge_sha.clone().or_else(|| job.merge_sha.clone())?;
    let base_sha = result.base_sha.clone().or_else(|| job.base_sha.clone())?;
    let fetch_remote = lane.fetch_remote.clone()?;
    // The home host needs a checkout of its own to push from. The project's
    // own home-host lane is the natural one; fall back to any lane pinned to
    // the home host.
    let home_repo = project
        .gate
        .lanes
        .iter()
        .find(|candidate| candidate.host_id == home_host)
        .map(|candidate| candidate.repo_path.clone())?;
    Some(HomeLandPlan {
        home_host,
        params: remuda_protocol::GateLandParams {
            job_id: job.id.as_id().to_string(),
            repo_path: home_repo,
            branch: job.branch.clone(),
            base_branch: project.default_base_branch.clone(),
            push_remote: "origin".into(),
            fetch_remote,
            merge_ref,
            merge_sha,
            base_sha,
            timeout_secs: 0,
        },
        lane_host: lane.host_id.clone(),
        lane_repo: lane.repo_path.clone(),
    })
}

/// Drive `gate.land` on the project home host and write the terminal state.
///
/// The three outcomes map straight onto D-034: `landed` finishes the job and
/// runs `--then`; `base-moved` re-queues a verify under the same attempt cap
/// (nothing was pushed); anything else fails the job. There is no state in
/// which main moved but the job did not finish, because the push itself is a
/// single leased update.
async fn land_from_home(state: &AppState, job_id: &str, plan: HomeLandPlan) {
    // Claim the push: mark the home land in flight so a cancel arriving during
    // it cannot be finalized (or have its pinned merge dropped) by the bounded
    // cancel grace — the push can take minutes, and land_from_home writes the
    // honest terminal state itself. A job no longer `running` was already
    // finished (canceled or otherwise); never push it.
    let claimed = state
        .store
        .mutate_gate_job(job_id, |row, _conn| {
            if row.state != GateJobState::Running {
                return None;
            }
            row.home_land_in_flight = true;
            Some(())
        })
        .await;
    if !matches!(claimed, Ok(Some(_))) {
        return;
    }
    let params = serde_json::to_value(&plan.params).unwrap_or(Value::Null);
    let reply = state
        .nodes
        .call(
            plan.home_host.as_id().as_str(),
            remuda_protocol::METHOD_GATE_LAND,
            params,
            HOME_LAND_TIMEOUT,
        )
        .await;
    // Whether the Hub actually knows the push outcome. A read reply (landed /
    // base-moved / the home host's own failure) is definitive; a timeout or an
    // unreachable host is not — the push may have completed after we gave up,
    // so we must not assert main did not move.
    let mut push_outcome_unknown = false;
    let landed: remuda_protocol::GateLandResult = match reply {
        Ok(Some(frame)) => match frame.get("result") {
            Some(value) => {
                serde_json::from_value(value.clone()).unwrap_or(remuda_protocol::GateLandResult {
                    job_id: job_id.to_owned(),
                    status: "failed".into(),
                    error: Some("gate.land returned an unreadable result".into()),
                    ..Default::default()
                })
            }
            None => remuda_protocol::GateLandResult {
                job_id: job_id.to_owned(),
                status: "failed".into(),
                error: Some(
                    frame
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("gate.land failed")
                        .to_owned(),
                ),
                ..Default::default()
            },
        },
        // An unreachable home host leaves the verify pinned and landable by
        // hand; it is never a silent success.
        Ok(None) => {
            push_outcome_unknown = true;
            remuda_protocol::GateLandResult {
                job_id: job_id.to_owned(),
                status: "failed".into(),
                error: Some("home host is not connected; push outcome is unknown".into()),
                ..Default::default()
            }
        }
        Err(error) => {
            push_outcome_unknown = true;
            remuda_protocol::GateLandResult {
                job_id: job_id.to_owned(),
                status: "failed".into(),
                error: Some(format!("gate.land did not return: {error}")),
                ..Default::default()
            }
        }
    };
    let status = landed.status.clone();
    let outcome = state
        .store
        .mutate_gate_job(job_id, move |row, _conn| {
            // Only a job the push still owns may be written here. Once it is
            // already terminal, nothing to do (its flag is already clear).
            if !matches!(row.state, GateJobState::Running | GateJobState::Canceling) {
                return None;
            }
            // The home land is done; clear the in-flight marker whatever the
            // outcome so the cancel grace is free to act again.
            row.home_land_in_flight = false;
            // A cancel that arrived during the push finds the job `canceling`.
            // A `landed` result is still authoritative — main moved, so record
            // `landed` and note the cancel arrived too late (mirrors the
            // apply_result pushFrom-lane branch: honesty over tidiness). Any
            // other outcome ends `canceled`, keeping the land's own evidence;
            // when the push outcome is unknown (timeout / unreachable) we say
            // so rather than assert main did not move.
            if row.state == GateJobState::Canceling {
                if landed.status == "landed" {
                    row.evidence_uncertain = None;
                    row.state = GateJobState::Landed;
                    row.finished_at = Some(now_ts());
                    row.merge_sha = landed.merge_sha.clone().or(row.merge_sha.clone());
                    row.error = None;
                    row.reason = Some("cancel arrived after the home host pushed main".into());
                } else {
                    row.evidence_uncertain = None;
                    row.state = GateJobState::Canceled;
                    row.finished_at = Some(now_ts());
                    row.current_main_sha = landed
                        .current_main_sha
                        .clone()
                        .or(row.current_main_sha.clone());
                    let base = landed
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("home-host land ended: {}", landed.status));
                    if push_outcome_unknown {
                        row.error = Some(format!("canceled; push outcome unknown: {base}"));
                        row.reason = Some("canceled; push outcome unknown".into());
                    } else {
                        row.error = Some(format!("canceled; not landed: {base}"));
                        row.reason = Some("canceled; the home host did not push main".into());
                    }
                }
                return Some(());
            }
            match landed.status.as_str() {
                "landed" => {
                    row.evidence_uncertain = None;
                    row.state = GateJobState::Landed;
                    row.finished_at = Some(now_ts());
                    row.merge_sha = landed.merge_sha.clone().or(row.merge_sha.clone());
                    row.error = None;
                    row.reason = None;
                }
                "base-moved" if row.attempts < MAX_LAND_ATTEMPTS => {
                    row.evidence_uncertain = None;
                    row.state = GateJobState::Queued;
                    row.started_at = None;
                    row.host_id = None;
                    row.lane_id = None;
                    row.current_main_sha = landed.current_main_sha.clone();
                    // The next attempt re-verifies onto the new base and pins
                    // a fresh merge; this one's refs are dropped by the caller.
                    row.merge_ref = None;
                    row.merge_sha = None;
                    row.failed_step = None;
                    row.reason = None;
                    row.log_object_id = None;
                }
                other => {
                    row.evidence_uncertain = None;
                    row.state = GateJobState::Failed;
                    row.finished_at = Some(now_ts());
                    row.current_main_sha = landed.current_main_sha.clone();
                    let error = landed.error.clone().unwrap_or_else(|| {
                        if other == "base-moved" {
                            format!(
                                "main moved and the land attempt cap ({MAX_LAND_ATTEMPTS}) is spent"
                            )
                        } else {
                            format!("home-host land ended: {other}")
                        }
                    });
                    row.reason = Some(error.lines().next().unwrap_or(&error).to_owned());
                    row.error = Some(error);
                }
            }
            Some(())
        })
        .await;
    let Ok(Some(job)) = outcome else {
        return;
    };
    // Drop the lane's pinned refs once they can no longer be needed: the land
    // succeeded, or this attempt's merge is superseded by a re-verify.
    if matches!(job.state, GateJobState::Landed | GateJobState::Queued) {
        unpin_lane_refs(state, &plan.lane_host, &plan.lane_repo, job_id).await;
    }
    let action = match job.state {
        GateJobState::Landed => "gate.landed",
        GateJobState::Queued => "gate.base-moved-requeue",
        GateJobState::Canceled => "gate.canceled",
        _ => "gate.failed",
    };
    journal(state, &job.requested_by, action, &job).await;
    if job.state == GateJobState::Landed {
        // Only ever after the push actually succeeded.
        run_then_hook(state, &job).await;
    } else if status == "base-moved" {
        tracing::info!(
            job_id,
            "home-host land refused: base moved; re-queued a verify"
        );
    }
}

/// Drop a job's pinned refs, resolving the lane host from the job's project.
///
/// Used on cancel and after a lane-side land. A job with no `mergeRef` never
/// pinned anything, so this is a no-op for it.
async fn drop_job_refs(state: &AppState, job: &GateJob) {
    if job.merge_ref.is_none() {
        return;
    }
    let Some(lane_id) = &job.lane_id else {
        return;
    };
    let Ok(Some(project)) = state
        .store
        .get_project(job.project_id.as_id().to_string())
        .await
    else {
        return;
    };
    let Some(lane) = project.gate.lanes.iter().find(|lane| &lane.id == lane_id) else {
        return;
    };
    unpin_lane_refs(
        state,
        &lane.host_id,
        &lane.repo_path,
        job.id.as_id().as_str(),
    )
    .await;
    let _ = state
        .store
        .mutate_gate_job(job.id.as_id().as_str(), |row, _conn| {
            row.merge_ref = None;
            Some(())
        })
        .await;
}

/// Retention sweep: drop pinned refs for jobs that passed but were never
/// landed inside the configured window.
///
/// This is the backstop for the cases the direct paths miss — an operator who
/// walks away from a passing verify, or a lane that was offline when its job
/// was canceled. Terminal jobs only: a queued or running job may still land.
///
/// Self-throttled: a full scan runs at most once per interval, and the interval
/// is derived from the retention window so a short window (tests, an operator
/// tightening retention) is still honoured promptly instead of being rounded up
/// to the default minute.
pub(crate) async fn sweep_expired_refs(state: &AppState) {
    let retention_ms = state.config.gate_ref_retention_ms;
    if retention_ms == 0 {
        return;
    }
    let interval = REF_SWEEP_INTERVAL.min(Duration::from_millis(retention_ms.max(1)));
    {
        let mut last = state
            .gate_ref_swept_at
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if last.is_some_and(|at| at.elapsed() < interval) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let Ok(jobs) = state.store.list_gate_jobs(None).await else {
        return;
    };
    let now = now();
    for job in jobs {
        if job.merge_ref.is_none() {
            continue;
        }
        if !matches!(
            job.state,
            GateJobState::Passed | GateJobState::Failed | GateJobState::Canceled
        ) {
            continue;
        }
        let stamp = job
            .finished_at
            .clone()
            .unwrap_or_else(|| job.queued_at.clone());
        if age_ms(&now, &stamp).is_some_and(|age| age >= retention_ms) {
            drop_job_refs(state, &job).await;
        }
    }
}

/// Milliseconds between two RFC3339 timestamps, `None` if unparseable.
fn age_ms(now: &str, earlier: &Timestamp) -> Option<u64> {
    let parse = |value: &str| {
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
    };
    let now = parse(now)?;
    let earlier = parse(String::from(earlier.clone()).as_str())?;
    u64::try_from((now - earlier).whole_milliseconds()).ok()
}

/// Milliseconds between `now` and a RFC3339 timestamp string; None if either is
/// unparseable or the timestamp is in the future.
fn elapsed_since(now: &str, earlier: Option<&str>) -> Option<u64> {
    let earlier = earlier?;
    let parse = |value: &str| {
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
    };
    let now = parse(now)?;
    let earlier = parse(earlier)?;
    u64::try_from((now - earlier).whole_milliseconds()).ok()
}

/// Best-effort `gate.unpin` on the lane host.
async fn unpin_lane_refs(
    state: &AppState,
    lane_host: &remuda_protocol::HostId,
    repo_path: &str,
    job_id: &str,
) {
    let params = serde_json::to_value(remuda_protocol::GateUnpinParams {
        job_id: job_id.to_owned(),
        repo_path: repo_path.to_owned(),
    })
    .unwrap_or(Value::Null);
    if let Err(error) = state
        .nodes
        .call(
            lane_host.as_id().as_str(),
            remuda_protocol::METHOD_GATE_UNPIN,
            params,
            UNPIN_TIMEOUT,
        )
        .await
    {
        // Retention sweeps the ref later; losing this call is not fatal.
        tracing::debug!(%error, job_id, "gate.unpin did not complete");
    }
}

/// `land --then "<cmd>"`: run the post-land command on the project home host.
async fn run_then_hook(state: &AppState, job: &GateJob) {
    let Some(command) = &job.then_command else {
        return;
    };
    let Some(project) = state
        .store
        .get_project(job.project_id.as_id().to_string())
        .await
        .ok()
        .flatten()
    else {
        return;
    };
    let Some(home) = &project.home_host else {
        return;
    };
    let params = json!({
        "jobId": job.id.as_id().as_str(),
        "command": command,
        "env": {},
        "timeoutSecs": 0,
    });
    let output = match state
        .nodes
        .call(
            home.as_id().as_str(),
            "gate.then",
            params,
            Duration::from_secs(12 * 60),
        )
        .await
    {
        Ok(Some(frame)) => frame
            .get("result")
            .and_then(|result| result.get("output"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| "gate.then returned no output".into()),
        Ok(None) => "home host is not connected; --then not run".into(),
        Err(error) => format!("gate.then failed: {error}"),
    };
    if let Ok(Some(updated)) = state
        .store
        .mutate_gate_job(job.id.as_id().as_str(), move |row, _conn| {
            row.then_output = Some(output);
            Some(())
        })
        .await
    {
        journal(state, &updated.requested_by, "gate.then", &updated).await;
    }
}

/// Mark orphaned running jobs (Hub restarted mid-run) back to queued.
pub(crate) async fn reconcile(state: &AppState) {
    let jobs = state.store.list_gate_jobs(None).await;
    let Ok(jobs) = jobs else {
        return;
    };
    for job in jobs
        .into_iter()
        .filter(|job| matches!(job.state, GateJobState::Running | GateJobState::Canceling))
        // r23 R9: a deliberately-parked evidence-uncertain job stays parked
        // across a Hub restart, holding its occupancy for the operator: the
        // restart requeue must not auto-run a fresh attempt over an
        // unconsumed definitive verdict (no Hub-side retry scheduler). The
        // operator resolve path works with no carrier.
        .filter(|job| job.evidence_uncertain.is_none())
    {
        let _ = state
            .store
            .mutate_gate_job(job.id.as_id().as_str(), |row, _conn| {
                // The home push (if any) did not survive the restart: no task
                // is holding it, so clear the marker or it would wedge the
                // cancel grace forever on a re-queued or re-cancelable job.
                row.home_land_in_flight = false;
                if row.state == GateJobState::Canceling {
                    // A cancel was already requested; do not silently drop it
                    // back into the queue. Finish it canceled.
                    row.state = GateJobState::Canceled;
                    row.finished_at = Some(now_ts());
                    row.error =
                        Some("hub restarted while the job was canceling; finished canceled".into());
                    row.reason = Some("canceled; hub restarted mid-cancel".into());
                    return Some(());
                }
                row.state = GateJobState::Queued;
                row.started_at = None;
                row.host_id = None;
                row.lane_id = None;
                row.error = Some("hub restarted while the job was running; re-queued".into());
                Some(())
            })
            .await;
    }
    // r13 item 1: Landing rows are NOT settled at startup. The carrier registry
    // is empty before the listener binds, so the remote cannot be probed yet,
    // and guessing a verdict here is exactly the bug: a push that reached
    // origin/main before the Hub exited would be called failed, and a push that
    // is still running could be re-run (a second push). Every Landing row keeps
    // its lane/land occupancy and is reconciled only once a carrier can answer
    // (`reconcile_landings` after hello and on every schedule tick) against the
    // EXACT granted merge, bounded by the recovery window.
}

/// A persisted Landing row plus its Hub-only recovery bookkeeping.
pub(crate) struct LandingRow {
    job: GateJob,
    landing_at: Option<String>,
    probed_at: Option<String>,
    /// The authenticated carrier session the LIVE grant belongs to (Hub-only
    /// column). A read-only probe settles nothing unless the row it lands on is
    /// still the same granted (attempt, merge, session).
    grant_session: Option<i64>,
}

/// Force one bounded Landing reconciliation pass for a host (or all hosts) off
/// whatever task noticed the need (an RPC/carrier loss, a reconnect). Detached
/// so the caller never blocks on a remote probe; the read-only probe is
/// self-throttling and never re-runs a push. r13 items 1 & 2.
pub(crate) fn schedule_landing_reconcile(state: &AppState, host: Option<String>) {
    let state = state.clone();
    tokio::spawn(async move {
        reconcile_landings(&state, host.as_deref()).await;
    });
}

fn message_or_default(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("gate.run failed")
        .to_owned()
}

/// Spawn the self-serializing Landing reconciler: one pass at a time, at the
/// configured probe interval, so an outstanding remote probe can never pile up
/// with another. Started by the Hub after it is serving; Landing rows only
/// resolve once a carrier is reachable. Forced passes (post-hello, post-loss)
/// run independently and bypass the per-row throttle. r13 items 1 & 2.
pub(crate) fn spawn_landing_reconciler(state: &AppState) -> tokio::task::JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        let interval_ms = state
            .config
            .gate_landing_probe_interval_ms
            .max(MIN_LANDING_PROBE_INTERVAL_MS);
        let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
        loop {
            interval.tick().await;
            reconcile_landings(&state, None).await;
        }
    })
}

/// Reconcile every Landing row against the remote, read-only, WITHOUT ever
/// re-running a push. Called from the schedule tick (throttled) and forced for
/// one host immediately after its carrier registers (`force_host`).
///
/// r14 R1 — outcome rule for a row whose lane host is currently connected:
///   * the exact granted merge is an ancestor of the remote base →
///     LANDED (exactly once; the row keeps occupying until then);
///   * a fresh fetch proves the merge absent AND the lane attests the land
///     process group verified dead → re-queued (or failed at the attempt cap),
///     because nothing was pushed and nothing can still push;
///   * anything not established (push possibly in flight, host between
///     reconnects, a probe error, or an expired window) → the row STAYS Landing
///     and keeps its occupancy; once the bounded recovery window has elapsed it
///     is parked in the explicit operator-visible outcome-unknown state (flag +
///     reason + `gate.landing-unknown` journal) until a later probe establishes
///     a fact or the operator resolves it. Elapsed time alone never fails it.
pub(crate) async fn reconcile_landings(state: &AppState, force_host: Option<&str>) {
    let Ok(rows) = state.store.list_landing_jobs().await else {
        return;
    };
    let recovery_ms = state.config.gate_landing_recovery_ms;
    let probe_interval_ms = state
        .config
        .gate_landing_probe_interval_ms
        .max(MIN_LANDING_PROBE_INTERVAL_MS);
    for row in rows {
        let LandingRow {
            job,
            landing_at,
            probed_at,
            grant_session,
        } = row;
        let Some(lane_host) = landing_lane_host(state, &job).await else {
            continue;
        };
        // r15 item 3 (R2): only probe/settle through the host's CURRENT carrier
        // session, and pin it with the shared authority guard so a hello
        // replacement cannot land between the probe and the probe-settlement
        // commit. The carried responder identity is validated at commit.
        let _host_decision = state
            .nodes
            .host_authority(lane_host.as_id().as_str())
            .await
            .read_owned()
            .await;
        let forced = force_host.is_some_and(|host| host == lane_host.as_id().as_str());
        let now = now();
        let expired =
            elapsed_since(&now, landing_at.as_deref()).is_some_and(|age| age >= recovery_ms);
        // r15 item 5: evaluate expiry BEFORE the connectivity check. A
        // disconnected carrier must not strand the row in a plain Landing that
        // the operator resolve endpoint never accepts. Mark the EXACT attempt
        // (state + attempts predicate) outcome-unknown once the window has
        // elapsed; occupancy (host/lane assignment) is untouched and the
        // operator resolve path stays available while the host is away.
        let connected_session = state.nodes.generation_of(lane_host.as_id().as_str()).await;
        let Some(responder_session) = connected_session else {
            if expired && !job.landing_outcome_unknown {
                mark_landing_unknown(
                    state,
                    &job,
                    "the granted push was not confirmed before the carrier disconnected and its \
                     outcome could not be established; waiting for an operator resolution or a \
                     reconnect probe",
                )
                .await;
            }
            continue;
        };
        if !forced
            && let Some(age) = elapsed_since(&now, probed_at.as_deref())
            && age < probe_interval_ms
        {
            continue;
        }
        // r14 R1: only an ESTABLISHED fact moves a Landing. A probe says one
        // of three things — the exact merge is on the remote (landed); it is
        // provably absent AND the land group is verified dead (absent); or it
        // cannot establish either (unknown). Elapsed time by itself is never a
        // verdict: an expired unconfirmed Landing stays Landing in an explicit
        // outcome-unknown state, keeps its occupancy, and is surfaced.
        let outcome = probe_landing_outcome(state, &job).await;
        let _ = state
            .store
            .mark_landing_probed(job.id.as_id().as_str(), &now)
            .await;
        match outcome {
            LandingProbe::Landed => {
                settle_landing_landed(state, &job, grant_session, responder_session, &lane_host)
                    .await
            }
            LandingProbe::Absent => {
                // The lane only attests groups IT spawned. A `pushFrom: home`
                // push is driven by the home host, whose process liveness the
                // lane cannot see; there the `absent` answer is not
                // authoritative and the row stays Landing/unknown.
                if job.home_land_in_flight {
                    if expired {
                        mark_landing_unknown(
                            state,
                            &job,
                            "not confirmed on the remote; the home-host land group liveness is \
                             not attestable from the lane",
                        )
                        .await;
                    }
                } else {
                    // Established: nothing was pushed and the only process group
                    // that could push is verified dead — safe to re-queue (bounded
                    // by the same attempt cap) or fail at the cap.
                    settle_landing_absent(
                        state,
                        &job,
                        grant_session,
                        responder_session,
                        &lane_host,
                    )
                    .await;
                }
            }
            LandingProbe::Unknown => {
                if expired {
                    mark_landing_unknown(
                        state,
                        &job,
                        "the granted push was not confirmed on the remote base and its outcome \
                         could not be established",
                    )
                    .await;
                }
            }
        }
    }
}

/// Tell the lane Node to terminate the granted land group of a Landing the Hub
/// settled WITHOUT a landed push (provably absent, or operator-resolved), so
/// the lane frees for a fresh attempt and an orphaned group cannot push after
/// the verdict. Best-effort and bounded: an unreachable lane keeps its settled
/// row anyway, and the Node's startup fence kills any orphaned group at its next
/// boot. r14 R1.
async fn abort_landing_run(state: &AppState, job: &GateJob) {
    let (Some(host), Some(_lane)) = (&job.host_id, &job.lane_id) else {
        return;
    };
    let params = serde_json::to_value(remuda_protocol::GateAbortLandingParams {
        job_id: job.id.as_id().to_string(),
        generation: job.attempts,
    })
    .unwrap_or(Value::Null);
    let _ = state
        .nodes
        .call(
            host.as_id().as_str(),
            remuda_protocol::METHOD_GATE_ABORT_LANDING,
            params,
            Duration::from_secs(10),
        )
        .await;
}

/// Resolve the lane host a Landing job must be probed on, if its project/lane
/// still resolve.
async fn landing_lane_host(state: &AppState, job: &GateJob) -> Option<remuda_protocol::HostId> {
    let lane_id = job.lane_id.clone()?;
    let project = state
        .store
        .get_project(job.project_id.as_id().to_string())
        .await
        .ok()
        .flatten()?;
    project
        .gate
        .lanes
        .iter()
        .find(|lane| lane.id == lane_id)
        .map(|lane| lane.host_id.clone())
}

/// One fact a read-only Landing probe established.
enum LandingProbe {
    /// The exact granted merge is an ancestor of the remote base.
    Landed,
    /// A fresh fetch says the merge is absent AND the lane attests the land
    /// process group is verified dead.
    Absent,
    /// Nothing could be established (host/probe error, group possibly alive,
    /// or non-ancestry without a dead-group attestation).
    Unknown,
}

/// Mark an expired, unconfirmed Landing as explicitly OUTCOME UNKNOWN. The row
/// deliberately stays `landing`, keeps holding its lane/land occupancy, and is
/// surfaced to the operator (the `landingOutcomeUnknown` flag + reason on the
/// job and a one-shot `gate.landing-unknown` journal entry). Probing keeps
/// running; only an established remote fact or an operator resolution settles
/// the row. r14 R1.
async fn mark_landing_unknown(state: &AppState, job: &GateJob, note: &str) {
    let flipped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flipped_writer = flipped.clone();
    let note = note.to_owned();
    // r15 item 5: mark the EXACT attempt — a newer attempt that reclaimed the
    // job slot between the list read and this write must not be flagged.
    let expected_attempts = job.attempts;
    let outcome = state
        .store
        .mutate_gate_job(job.id.as_id().as_str(), move |row, _conn| {
            if row.state != GateJobState::Landing || row.attempts != expected_attempts {
                return None;
            }
            if !row.landing_outcome_unknown {
                row.landing_outcome_unknown = true;
                row.reason = Some(format!(
                    "landing outcome unknown after the bounded recovery window: {note}; the job \
                     keeps its landing occupancy until the remote outcome is established or an \
                     operator resolves it"
                ));
                flipped_writer.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Some(())
        })
        .await;
    if let Ok(Some(updated)) = outcome
        && flipped.load(std::sync::atomic::Ordering::SeqCst)
    {
        journal(
            state,
            &updated.requested_by,
            "gate.landing-unknown",
            &updated,
        )
        .await;
    }
}

/// Settle a Landing whose exact granted merge the probe confirmed on the
/// remote. Never re-queues or re-pushes. R2: the probe settles ONLY inside one
/// transaction matching the live granted attempt it ran against — same
/// (host, attempts, merge, session). `responder_session` is the authenticated
/// carrier session the probe reply rode, validated against the persisted grant
/// at commit (r15 item 3).
async fn settle_landing_landed(
    state: &AppState,
    job: &GateJob,
    grant_session: Option<i64>,
    responder_session: u64,
    host: &remuda_protocol::HostId,
) {
    let expected_session = probe_expected_session(state, grant_session, responder_session);
    let outcome = state
        .store
        .settle_landing_probe(job, expected_session, host.as_id().as_str(), true)
        .await;
    if let Ok(Some(settled)) = outcome {
        journal(state, &settled.requested_by, "gate.landed", &settled).await;
        drop_job_refs(state, &settled).await;
        run_then_hook(state, &settled).await;
    }
}

/// Settle a Landing the probe established ABSENT: a fresh remote fetch does not
/// contain the granted merge AND the lane verified the land process group dead.
/// Nothing was pushed and nothing can still push, so the job re-enters the queue
/// for a fresh attempt, or fails once the attempt cap is reached. R2: only the
/// live granted attempt (host/attempts/merge/session unchanged through the
/// probe) is re-queued this way.
async fn settle_landing_absent(
    state: &AppState,
    job: &GateJob,
    grant_session: Option<i64>,
    responder_session: u64,
    host: &remuda_protocol::HostId,
) {
    let expected_session = probe_expected_session(state, grant_session, responder_session);
    let outcome = state
        .store
        .settle_landing_probe(job, expected_session, host.as_id().as_str(), false)
        .await;
    if let Ok(Some(settled)) = outcome {
        // Normally the run is already gone (the probe required a verified-dead
        // group); make sure before re-dispatching or dropping refs.
        abort_landing_run(state, &settled).await;
        match settled.state {
            GateJobState::Queued => {
                journal(
                    state,
                    &settled.requested_by,
                    "gate.base-moved-requeue",
                    &settled,
                )
                .await;
                schedule_once(state);
            }
            GateJobState::Failed => {
                journal(state, &settled.requested_by, "gate.failed", &settled).await;
                drop_job_refs(state, &settled).await;
            }
            _ => {}
        }
    }
}
/// The session the probe-settlement writer must match against the row's
/// `dispatch_session` (r15 item 3):
/// * a grant minted by THIS Hub process can only be settled by the exact
///   CURRENT carrier session the authenticated probe rode — `responder`;
/// * a grant minted by a PREVIOUS Hub process (its high 32 bits differ from
///   this boot epoch, e.g. after a Hub restart) can never be live again, so the
///   row keeps naming that stale session while the current authenticated host
///   establishes the remote fact (this preserves restart Landing recovery).
fn probe_expected_session(
    state: &AppState,
    grant_session: Option<i64>,
    responder_session: u64,
) -> Option<i64> {
    let boot_epoch = state.nodes.boot_epoch();
    match grant_session {
        Some(grant) if u64::try_from(grant).unwrap_or(0) >> 32 == boot_epoch => {
            Some(i64::try_from(responder_session).unwrap_or(0))
        }
        other => other,
    }
}

pub(crate) async fn recover_ended_session(state: &AppState, host_id: &str, session: u64) {
    if session == 0 {
        return;
    }
    let recovered = state.store.recover_ended_gate_jobs(host_id, session).await;
    let Ok(jobs) = recovered else {
        return;
    };
    for job in &jobs {
        if job.state == GateJobState::Canceled {
            journal(state, &job.requested_by, "gate.canceled", job).await;
            drop_job_refs(state, job).await;
        }
    }
    if !jobs.is_empty() {
        schedule_once(state);
    }
}

/// Ask the lane host (read-only) what the remote AND its local land-process
/// liveness establish for the granted merge:
/// * `Landed` — exact merge is an ancestor of the remote base;
/// * `Absent` — fresh fetch, merge not on the remote, AND the Node attests the
///   land process group that could still push is verified gone;
/// * `Unknown` — host gone, RPC/fetch error, or still possibly in flight.
async fn probe_landing_outcome(state: &AppState, job: &GateJob) -> LandingProbe {
    let Some(merge_sha) = job.merge_sha.clone() else {
        return LandingProbe::Unknown;
    };
    let Some(lane_id) = job.lane_id.clone() else {
        return LandingProbe::Unknown;
    };
    let Ok(Some(project)) = state
        .store
        .get_project(job.project_id.as_id().to_string())
        .await
    else {
        return LandingProbe::Unknown;
    };
    let Some(lane) = project.gate.lanes.iter().find(|lane| lane.id == lane_id) else {
        return LandingProbe::Unknown;
    };
    let params = serde_json::to_value(remuda_protocol::GateResolveLandingParams {
        job_id: job.id.as_id().to_string(),
        repo_path: lane.repo_path.clone(),
        base_branch: project.default_base_branch.clone(),
        merge_sha,
    })
    .unwrap_or(Value::Null);
    let reply = state
        .nodes
        .call(
            lane.host_id.as_id().as_str(),
            remuda_protocol::METHOD_GATE_RESOLVE_LANDING,
            params,
            Duration::from_secs(30),
        )
        .await;
    let status = reply.ok().flatten().and_then(|frame| {
        frame
            .get("result")
            .and_then(|result| result.get("status"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    match status.as_deref() {
        Some("landed") => LandingProbe::Landed,
        Some("absent") => LandingProbe::Absent,
        _ => LandingProbe::Unknown,
    }
}

// ── store CRUD ─────────────────────────────────────────────────────────────

impl Store {
    pub(crate) async fn insert_gate_job(
        &self,
        job: &GateJob,
    ) -> Result<(), crate::store::StoreError> {
        let doc = serde_json::to_string(job)?;
        let id = job.id.as_id().to_string();
        let project_id = job.project_id.as_id().to_string();
        let state = job.state.as_str().to_owned();
        let lane = job.lane_id.clone();
        let queued = String::from(job.queued_at.clone());
        let created = String::from(job.queued_at.clone());
        // D-057 §7.1: authority pair checked inside the writer job (§7.3);
        // the device id persists in its own gate_jobs column (it is
        // skip_serializing in the doc JSON) and never reaches the wire.
        let job_initiator = job.initiator.clone();
        let job_device_id = job.initiator_device_id.clone();
        self.run_named("insert_gate_job", move |conn| {
            // D-057 §7.3: gate job insert is an Agent-admitted write; the
            // job row is the gate.run admission record (main-agent.md §7.5).
            crate::store::check_initiator(conn, job_initiator.as_ref(), job_device_id.as_deref())?;
            conn.execute(
                "INSERT INTO gate_jobs
                    (id, project_id, state, lane_id, queued_at, doc_json, revision,
                     created_by, created_at, updated_at, initiator_device_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?8, ?9)",
                params![
                    id,
                    project_id,
                    state,
                    lane,
                    queued,
                    doc,
                    "",
                    created,
                    job_device_id
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_gate_job(
        &self,
        id: &str,
    ) -> Result<Option<GateJob>, crate::store::StoreError> {
        let id = id.to_owned();
        self.run_named("get_gate_job", move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT doc_json FROM gate_jobs WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(raw.and_then(|text| serde_json::from_str(&text).ok()))
        })
        .await
    }

    /// r13 item 4: the authenticated carrier session the live dispatch/grant
    /// is bound to (Hub-only column), so a verdict arriving on a replacement
    /// session can be compared against the GRANTED session inside the writer.
    pub(crate) async fn gate_job_dispatch_session(
        &self,
        job_id: &str,
    ) -> Result<Option<i64>, crate::store::StoreError> {
        let job_id = job_id.to_owned();
        self.run_named("gate_job_dispatch_session", move |conn| {
            let value: Option<Option<i64>> = conn
                .query_row(
                    "SELECT dispatch_session FROM gate_jobs WHERE id = ?1",
                    params![job_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(value.flatten())
        })
        .await
    }

    /// r13 item 1: list every Landing row together with its grant timestamp
    /// and last remote-probe timestamp (both Hub-only columns).
    pub(crate) async fn list_landing_jobs(
        &self,
    ) -> Result<Vec<LandingRow>, crate::store::StoreError> {
        self.run_named("list_landing_jobs", |conn| {
            let mut statement = conn.prepare(
                "SELECT doc_json, landing_at, landing_probed_at, dispatch_session
                  FROM gate_jobs
                  WHERE state = 'landing' ORDER BY queued_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map([], |row| {
                let doc: String = row.get(0)?;
                let landing_at: Option<String> = row.get(1)?;
                let probed_at: Option<String> = row.get(2)?;
                let grant_session: Option<i64> = row.get(3)?;
                Ok((doc, landing_at, probed_at, grant_session))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (doc, landing_at, probed_at, grant_session) = row?;
                if let Ok(job) = serde_json::from_str::<GateJob>(&doc) {
                    out.push(LandingRow {
                        job,
                        landing_at,
                        probed_at,
                        grant_session,
                    });
                }
            }
            Ok(out)
        })
        .await
    }

    /// r13 item 1: stamp the last remote-probe time for one Landing row.
    pub(crate) async fn mark_landing_probed(
        &self,
        job_id: &str,
        at: &str,
    ) -> Result<(), crate::store::StoreError> {
        let job_id = job_id.to_owned();
        let at = at.to_owned();
        self.run_named("mark_landing_probed", move |conn| {
            conn.execute(
                "UPDATE gate_jobs SET landing_probed_at = ?2 WHERE id = ?1 AND state = 'landing'",
                params![job_id, at],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn list_gate_jobs(
        &self,
        project_id: Option<&str>,
    ) -> Result<Vec<GateJob>, crate::store::StoreError> {
        let project_id = project_id.map(str::to_owned);
        self.run_named("list_gate_jobs", move |conn| {
            let mut jobs = Vec::new();
            if let Some(project) = &project_id {
                let mut statement = conn.prepare(
                    "SELECT doc_json FROM gate_jobs WHERE project_id = ?1 \
                     ORDER BY queued_at ASC, rowid ASC",
                )?;
                let rows = statement.query_map(params![project], |row| row.get::<_, String>(0))?;
                for row in rows {
                    if let Ok(job) = serde_json::from_str::<GateJob>(&row?) {
                        jobs.push(job);
                    }
                }
            } else {
                let mut statement = conn
                    .prepare("SELECT doc_json FROM gate_jobs ORDER BY queued_at ASC, rowid ASC")?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                for row in rows {
                    if let Ok(job) = serde_json::from_str::<GateJob>(&row?) {
                        jobs.push(job);
                    }
                }
            }
            Ok(jobs)
        })
        .await
    }

    /// Load → mutate → persist; returns None when the row is missing. The
    /// closure returning `Err` aborts the update.
    pub(crate) async fn mutate_gate_job<F>(
        &self,
        id: &str,
        mutate: F,
    ) -> Result<Option<GateJob>, crate::store::StoreError>
    where
        F: FnOnce(&mut GateJob, &rusqlite::Connection) -> Option<()> + Send + 'static,
    {
        let id = id.to_owned();
        // Test-only seam: a cancel request commits Running -> Canceling
        // before this writer loads the doc (drained outside, applied inside).
        #[cfg(any(test, feature = "test-faults"))]
        let armed_cancel = self.take_test_cancel_before_gate_mutate();
        // Test-only seam: a duplicate verdict's first arrival (or the
        // cancel-grace finisher) commits Canceling -> Canceled before this
        // writer loads the doc.
        #[cfg(any(test, feature = "test-faults"))]
        let armed_finish = self.take_test_finish_before_gate_mutate();
        self.run_named("mutate_gate_job", move |conn| {
            #[cfg(any(test, feature = "test-faults"))]
            if let Some(cancel_job) = armed_cancel
                && cancel_job == id
            {
                // Same transition request_gate_job_cancel commits: state,
                // stamped doc fields and revision, only for a still-Running
                // job (a terminal row is never dragged back).
                conn.execute(
                    "UPDATE gate_jobs
                        SET state = 'canceling',
                            doc_json = json_set(
                                json_set(doc_json, '$.state', 'canceling'),
                                '$.cancelRequestedAt', ?2),
                            revision = revision + 1,
                            updated_at = ?2
                      WHERE id = ?1 AND state = 'running'",
                    params![id, now()],
                )?;
            }
            #[cfg(any(test, feature = "test-faults"))]
            if let Some((finished_job, kind)) = armed_finish
                && finished_job == id
            {
                match kind {
                    crate::gatequeue::TestGateFinishBeforeWrite::DuplicateFailed => {
                        // The first duplicate's terminal write: Running ->
                        // Failed + finishedAt, preserving whatever logObjectId
                        // that first writer stored.
                        conn.execute(
                            "UPDATE gate_jobs
                                SET state = 'failed',
                                    doc_json = json_set(
                                        json_set(doc_json, '$.state', 'failed'),
                                        '$.finishedAt', ?2),
                                    revision = revision + 1,
                                    updated_at = ?2
                              WHERE id = ?1 AND state = 'running'",
                            params![id, now()],
                        )?;
                    }
                    _ => {
                        // The terminal write that lands before this writer
                        // loads: state canceled + finishedAt, and the same
                        // evidence the real interloper commits (the
                        // duplicate's first arrival via the Canceling branch,
                        // or finish_expired_cancels' text).
                        let (error, reason) = match kind {
                            crate::gatequeue::TestGateFinishBeforeWrite::DuplicateVerdict => (
                                "canceled; run ended base-moved",
                                "canceled; run ended base-moved",
                            ),
                            crate::gatequeue::TestGateFinishBeforeWrite::GraceExpiry => (
                                "canceled; the lane did not answer within the cancel grace",
                                "canceled; run did not stop in time",
                            ),
                            crate::gatequeue::TestGateFinishBeforeWrite::DuplicateFailed => {
                                unreachable!("handled above")
                            }
                        };
                        conn.execute(
                            "UPDATE gate_jobs
                                SET state = 'canceled',
                                    doc_json = json_set(
                                        json_set(
                                            json_set(
                                                json_set(doc_json, '$.state', 'canceled'),
                                                '$.finishedAt', ?2),
                                            '$.error', ?3),
                                        '$.reason', ?4),
                                    revision = revision + 1,
                                    updated_at = ?2
                              WHERE id = ?1 AND state = 'canceling'",
                            params![id, now(), error, reason],
                        )?;
                    }
                }
            }
            let Some(mut job) = conn
                .query_row(
                    "SELECT doc_json FROM gate_jobs WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .and_then(|text| serde_json::from_str::<GateJob>(&text).ok())
            else {
                return Ok(None);
            };
            if mutate(&mut job, conn).is_none() {
                return Ok(None);
            }
            let doc = serde_json::to_string(&job)?;
            let now = now();
            conn.execute(
                "UPDATE gate_jobs
                    SET doc_json = ?1, state = ?2, lane_id = ?3, revision = revision + 1,
                        updated_at = ?4
                  WHERE id = ?5",
                params![doc, job.state.as_str(), job.lane_id, now, id],
            )?;
            Ok(Some(job))
        })
        .await
    }

    /// D-057 §7.3: the HTTP cancel route only. `check_initiator` and the row
    /// transition run in ONE writer job, before any `gate.cancel` frame is
    /// sent, so a request authenticated before a fence cannot cancel after
    /// it. The scheduler's own `mutate_gate_job` callers (grace expiry,
    /// reconcile, a started job's follow-ons) deliberately do not call this.
    ///
    /// The outcome branches on the state observed INSIDE this writer job:
    /// the handler's pre-read is stale by definition (the 1 s scheduler
    /// tick or a finishing dispatch can move the row in between), so the
    /// decision to send `gate.cancel` is made here, never from the read.
    pub(crate) async fn request_gate_job_cancel(
        &self,
        id: &str,
        authority: crate::agent_scope::CallerAuthority,
    ) -> Result<Option<GateJobCancelOutcome>, crate::store::StoreError> {
        let id = id.to_owned();
        #[cfg(any(test, feature = "test-faults"))]
        let armed_race = self.take_test_gate_cancel_race();
        self.run_named("request_gate_job_cancel", move |conn| {
            let (initiator, device_id) = authority.as_check();
            crate::store::check_initiator(conn, initiator, device_id)?;
            // Test-only seam: a scheduler claim / dispatch finish that
            // commits between the handler's pre-read and this read.
            #[cfg(any(test, feature = "test-faults"))]
            if let Some(race) = armed_race {
                race.apply(conn, &id)?;
            }
            let Some(mut job) = conn
                .query_row(
                    "SELECT doc_json FROM gate_jobs WHERE id = ?1",
                    params![id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .and_then(|text| serde_json::from_str::<GateJob>(&text).ok())
            else {
                return Ok(None);
            };
            let prior = job.state;
            match job.state {
                GateJobState::Queued => {
                    job.state = GateJobState::Canceled;
                    job.finished_at = Some(now_ts());
                    job.error = Some("canceled while queued".into());
                }
                GateJobState::Running | GateJobState::Canceling => {
                    job.state = GateJobState::Canceling;
                    // Stamp the first cancel request; the scheduler finishes
                    // the job canceled after a bounded grace even if the Node
                    // never answers.
                    if job.cancel_requested_at.is_none() {
                        job.cancel_requested_at = Some(now_ts());
                    }
                }
                GateJobState::Landing => return Ok(Some(GateJobCancelOutcome::Landing(job))),
                // Terminal: leave the row untouched; the handler returns 409.
                _ => return Ok(Some(GateJobCancelOutcome::Terminal(job))),
            }
            let doc = serde_json::to_string(&job)?;
            conn.execute(
                "UPDATE gate_jobs
                    SET doc_json = ?1, state = ?2, lane_id = ?3, revision = revision + 1,
                        updated_at = ?4
                  WHERE id = ?5",
                params![doc, job.state.as_str(), job.lane_id, now(), id],
            )?;
            Ok(Some(match prior {
                GateJobState::Queued => GateJobCancelOutcome::QueuedCanceled(job),
                GateJobState::Running | GateJobState::Canceling => {
                    GateJobCancelOutcome::CancelRequested(job)
                }
                _ => unreachable!("terminal rows return above"),
            }))
        })
        .await
    }

    /// D-057 §7.3/§7.5: claim a queued gate job for dispatch, re-checking the
    /// initiator STAMPED ON THE ROW (and the device id persisted in its own
    /// column) inside the writer job. A job fenced after enqueue is not
    /// claimed: it settles `canceled` with reason `fenced` in the same job
    /// ([`GateJobClaim::FencedCanceled`], so dispatch can journal it) and no
    /// gate.run reaches the lane. Human/Bot jobs (`initiator: None`) claim
    /// exactly as before. Non-`Fenced` store errors propagate for a retry on
    /// the next tick instead of claiming the job anyway.
    pub(crate) async fn claim_gate_job(
        &self,
        id: &str,
        claim_lane: String,
        claim_host: remuda_protocol::HostId,
    ) -> Result<GateJobClaim, crate::store::StoreError> {
        let id = id.to_owned();
        // Test-only seam: fence lands inside this writer job.
        #[cfg(any(test, feature = "test-faults"))]
        let armed_fence = self.take_test_authority_fence();
        self.run_named("claim_gate_job", move |conn| {
            #[cfg(any(test, feature = "test-faults"))]
            if let Some(fenced_instance) = armed_fence {
                crate::store::test_apply_fence(conn, &fenced_instance)?;
            }
            // The device id is read from its own column: the doc field is
            // skip_serializing, so doc_json never carries it.
            let Some((mut job, device_id)) = conn
                .query_row(
                    "SELECT doc_json, initiator_device_id FROM gate_jobs WHERE id = ?1",
                    params![id],
                    |row| {
                        let doc: String = row.get(0)?;
                        let device_id: Option<String> = row.get(1)?;
                        Ok((doc, device_id))
                    },
                )
                .optional()?
                .and_then(|(doc, device_id)| {
                    serde_json::from_str::<GateJob>(&doc)
                        .ok()
                        .map(|job| (job, device_id))
                })
            else {
                return Ok(GateJobClaim::NotReady);
            };
            if job.state != GateJobState::Queued {
                return Ok(GateJobClaim::NotReady);
            }
            if let Err(error) =
                crate::store::check_initiator(conn, job.initiator.as_ref(), device_id.as_deref())
            {
                match error {
                    crate::store::StoreError::Fenced => {
                        job.state = GateJobState::Canceled;
                        job.finished_at = Some(now_ts());
                        job.reason = Some("fenced".into());
                        let doc = serde_json::to_string(&job)?;
                        conn.execute(
                            "UPDATE gate_jobs
                                SET doc_json = ?1, state = ?2, lane_id = ?3, revision = revision + 1,
                                    updated_at = ?4
                              WHERE id = ?5",
                            params![doc, job.state.as_str(), job.lane_id, now(), id],
                        )?;
                        return Ok(GateJobClaim::FencedCanceled(job));
                    }
                    // A genuine storage failure must not be swallowed into a
                    // claim: propagate so the tick retries.
                    other => return Err(other),
                }
            }
            job.state = GateJobState::Running;
            job.lane_id = Some(claim_lane);
            job.host_id = Some(claim_host);
            job.started_at = Some(now_ts());
            job.attempts = job.attempts.saturating_add(1);
            let doc = serde_json::to_string(&job)?;
            conn.execute(
                "UPDATE gate_jobs
                    SET doc_json = ?1, state = ?2, lane_id = ?3, revision = revision + 1,
                        updated_at = ?4
                  WHERE id = ?5",
                params![doc, job.state.as_str(), job.lane_id, now(), id],
            )?;
            Ok(GateJobClaim::Claimed(job))
        })
        .await
    }
}

/// Outcome of [`Store::request_gate_job_cancel`].
pub(crate) enum GateJobCancelOutcome {
    /// The row was `queued` as observed INSIDE the writer: it is now
    /// terminal `canceled`; no `gate.cancel` frame is owed.
    QueuedCanceled(GateJob),
    /// The row was `running`/`canceling` inside the writer: it is now
    /// `canceling` and the handler owes one `gate.cancel` frame.
    CancelRequested(GateJob),
    /// The row was `landing` inside the writer: the irreversible push is
    /// granted and runs to completion. The cancel API reports
    /// landing-in-progress rather than a false cancellation.
    Landing(GateJob),
    /// The row was already terminal inside the writer; it was left
    /// untouched and the handler answers 409.
    Terminal(GateJob),
}

/// Test-only seam: a row transition that commits INSIDE the
/// [`Store::request_gate_job_cancel`] writer job, before its own read —
/// the deterministic equivalent of the 1 s scheduler tick claiming the
/// job (or the dispatch finishing it) between the handler's pre-read and
/// the writer. Drained after one fire.
#[doc(hidden)]
#[cfg(any(test, feature = "test-faults"))]
#[derive(Debug)]
pub enum TestGateCancelRace {
    /// The scheduler wins the queued job first: it is claimed to `running`
    /// and pinned to the lane before the cancel reads the row.
    Claimed { lane_id: String, host_id: String },
    /// A running job reaches a terminal state first (`"passed"`/`"failed"`/
    /// `"landed"`/`"canceled"`).
    Finished { state: &'static str },
}

/// Test-only seam: WHICH interloper commits the terminal row write inside a
/// `mutate_gate_job` writer before its doc loads (ma-initiator r7 item 1,
/// r8 item 1).
#[doc(hidden)]
#[cfg(any(test, feature = "test-faults"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestGateFinishBeforeWrite {
    /// The first of two duplicate verdicts (the Node's `finished` event and
    /// the gate.run reply carry the same base-moved result) settled the row
    /// Canceling -> Canceled.
    DuplicateVerdict,
    /// The cancel-grace finisher (`finish_expired_cancels`) settled it
    /// Canceling -> Canceled.
    GraceExpiry,
    /// The first of two duplicate FAILED verdicts settled the row
    /// Running -> Failed (carrying the persisted log object id); the second
    /// handler's pre-read still saw Running.
    DuplicateFailed,
}
#[cfg(any(test, feature = "test-faults"))]
impl TestGateCancelRace {
    fn apply(&self, conn: &rusqlite::Connection, id: &str) -> Result<(), crate::store::StoreError> {
        match self {
            Self::Claimed { lane_id, host_id } => {
                conn.execute(
                    "UPDATE gate_jobs
                        SET state = 'running', lane_id = ?1,
                            doc_json = json_set(json_set(json_set(doc_json,
                                '$.state', 'running'),
                                '$.laneId', ?1),
                                '$.hostId', ?2),
                            revision = revision + 1, updated_at = ?3
                      WHERE id = ?4",
                    params![lane_id, host_id, now(), id],
                )?;
            }
            Self::Finished { state } => {
                // Validate the wire spelling against the real enum; the
                // cancel transition only cares that it is terminal.
                let parsed = GateJobState::parse(state).ok_or_else(|| {
                    crate::store::StoreError::Id(format!("invalid test gate state {state}"))
                })?;
                if matches!(parsed, GateJobState::Queued | GateJobState::Running) {
                    return Err(crate::store::StoreError::Id(format!(
                        "test finish seam requires a terminal state, got {state}"
                    )));
                }
                conn.execute(
                    "UPDATE gate_jobs
                        SET state = ?1,
                            doc_json = json_set(doc_json, '$.state', ?1),
                            revision = revision + 1, updated_at = ?2
                      WHERE id = ?3",
                    params![state, now(), id],
                )?;
            }
        }
        Ok(())
    }
}

/// Outcome of [`Store::claim_gate_job`].
pub(crate) enum GateJobClaim {
    /// Claimed for this lane; gate.run may be sent.
    Claimed(GateJob),
    /// The row's initiator was fenced: the job was settled canceled(fenced)
    /// in the same writer job; no gate.run may be sent.
    FencedCanceled(GateJob),
    /// Missing, not queued, or otherwise not claimable this tick.
    NotReady,
}

// ── gate log objects ───────────────────────────────────────────────────────

/// Result of the Hub-side `gate.authorize` fence.
pub(crate) struct GateDispatchAuth {
    pub(crate) authorized: bool,
    pub(crate) state: Option<String>,
}

/// Result of the Running → Landing land-grant transition.
pub(crate) struct GateLandGrant {
    pub(crate) granted: bool,
    pub(crate) state: Option<String>,
    pub(crate) landing_in_progress: bool,
}

/// One persisted bounded gate log (`obj_…`); bytes are the JSON-serialized
/// `GateRunLog`. Separate from attachment objects: no instance binding, a
/// project-scoped reader, and a 30-day TTL.
pub(crate) struct GateLogObject {
    pub(crate) object_id: String,
    pub(crate) job_id: String,
    pub(crate) project_id: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) expires_at: String,
}

impl Store {
    /// Deterministic, CONTENT-ADDRESSED log-object id for one verdict's
    /// evidence (r19 item 4): `obj_<job>_<sha256>`. Every result gets its own
    /// immutable object, so two same-grant arrivals with different logs never
    /// overwrite one another and a settlement can only bind the object its own
    /// result persisted. The Node's duplicate delivery (a `finished` event and
    /// the `gate.run` reply) carries identical bytes, so both arrivals resolve
    /// to the same id and the second persist is an idempotent no-op.
    pub(crate) fn gate_log_object_id(job_id: &str, digest: &str) -> String {
        let suffix = job_id.strip_prefix("gjb_").unwrap_or(job_id);
        format!("obj_{suffix}_{digest}")
    }

    /// Persist one verdict's evidence as an IMMUTABLE, CONTENT-ADDRESSED log
    /// object (r19 item 4), returning its object id. Different results of one
    /// job get different rows and never overwrite one another; the settlement
    /// transaction binds the job to the id this call returned and verifies its
    /// digest. An identical dual delivery (finished event + RPC reply) resolves
    /// to the same id and is a no-op. A failed insert rolls back, creates no
    /// object, and leaves every earlier object served.
    pub(crate) async fn insert_gate_log(
        &self,
        project_id: &str,
        job_id: &str,
        bytes: Vec<u8>,
        digest: String,
        created_by: &str,
        auth: LogAuth,
        // r15 item 3: the authenticated host the evidence arrived on.
        host: Option<&str>,
    ) -> Result<Option<String>, crate::store::StoreError> {
        #![allow(clippy::too_many_arguments)]
        let project_id = project_id.to_owned();
        let job_id = job_id.to_owned();
        let created_by = created_by.to_owned();
        let source_session_i64 = auth.session.map(|s| i64::try_from(s).unwrap_or(0));
        let source_host = host.map(str::to_owned);
        let generation = auth.generation;
        // r16 item 5: the evidence must name the exact GRANTED merge; carried
        // into the Landing predicate below.
        let auth_merge = auth.merge_sha;
        // Drain the one-shot insert fault before the 'static writer closure.
        #[cfg(any(test, feature = "test-faults"))]
        let insert_fault = self.take_gate_log_insert_fault();
        self.run_named("insert_gate_log", move |conn| {
            // r23 item 2: enforce the final serialized-byte cap in RUNTIME
            // code, as defense in depth behind persisted_run_log_bytes. An
            // oversized object is rejected in every build, never persisted.
            if bytes.len() > GATE_LOG_MAX_BYTES {
                return Err(crate::store::StoreError::EvidenceTooLarge {
                    size: bytes.len(),
                    max: GATE_LOG_MAX_BYTES,
                });
            }
            let tx = crate::store::immediate_tx(conn)?;
            let now = now();
            let expires_at = gate_log_expiry(&now);
            // r12 item 8: read the row's current attempts INSIDE the same
            // immediate transaction as the delete+insert. A verdict whose
            // generation is no longer current (a genuine base-moved requeue
            // already claimed attempt N+1) must not replace the newer
            // attempt's evidence — the pre-read alone was racy across two
            // writers. BEGIN IMMEDIATE serializes this against the verdict and
            // claim writers, so the check and the replacement are atomic.
            let current_attempts: Option<u32> = tx
                .query_row(
                    "SELECT json_extract(doc_json, '$.attempts') FROM gate_jobs WHERE id = ?1",
                    params![job_id],
                    |row| row.get::<_, Option<u32>>(0),
                )
                .optional()?
                .flatten();
            if current_attempts.is_some_and(|attempts| attempts != generation) {
                tx.commit()?;
                return Ok(None);
            }
            // r19 item 4: read the row's current state and grant tuple INSIDE
            // this transaction, so the authority validation and the immutable
            // insert are atomic against the verdict settlement. Objects are
            // content-addressed, so no accepted-digest comparison is needed
            // here — a settlement binds its own object by digest.
            let (current_state, row_merge): (Option<String>, Option<String>) = tx
                .query_row(
                    "SELECT json_extract(doc_json, '$.state'),
                            json_extract(doc_json, '$.mergeSha')
                       FROM gate_jobs WHERE id = ?1",
                    params![job_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .unwrap_or((None, None));
            // The exact-grant tuple is required while the row is a LANDING
            // (session + host + merge) and, r17 item 3, on a settled
            // landed/failed land row too. A terminal settlement NULLS
            // dispatch_session by design, so there the surviving identity is
            // host + merge (both retained); a post-settlement frame from a
            // different host or for a different merge is refused. Evidence
            // CONTENT is separately immutable below.
            let source_or_sentinel = source_session_i64.unwrap_or(-1);
            // r16 item 5: the evidence is also bound to the GRANTED MERGE. A
            // Landing row always names its mergeSha; the write is refused
            // unless the evidence carries that exact non-empty sha, so a
            // mismatched (or merge-less) outcome's log is rejected BEFORE any
            // delete/insert. An empty auth merge is passed as NULL and never
            // matches.
            let auth_merge_nonempty = auth_merge.filter(|sha| !sha.trim().is_empty());
            if current_state.as_deref() == Some("landing") {
                let tuple_for_other: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM gate_jobs
                      WHERE id = ?1
                        AND (dispatch_session IS NULL
                             OR dispatch_session != ?2
                             OR (?3 IS NOT NULL AND json_extract(doc_json, '$.hostId') != ?3)
                             OR json_extract(doc_json, '$.mergeSha') IS NULL
                             OR ?4 IS NULL
                             OR json_extract(doc_json, '$.mergeSha') != ?4)",
                    params![job_id, source_or_sentinel, source_host, auth_merge_nonempty],
                    |row| row.get(0),
                )?;
                if tuple_for_other > 0 {
                    tx.commit()?;
                    return Ok(None);
                }
            } else if matches!(current_state.as_deref(), Some("landed" | "failed"))
                && row_merge.is_some()
            {
                // Settled land row: the bound session is nulled, so validate
                // the surviving host + granted merge.
                let terminal_for_other: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM gate_jobs
                      WHERE id = ?1
                        AND ((?2 IS NOT NULL AND json_extract(doc_json, '$.hostId') != ?2)
                             OR ?3 IS NULL
                             OR json_extract(doc_json, '$.mergeSha') != ?3)",
                    params![job_id, source_host, auth_merge_nonempty],
                    |row| row.get(0),
                )?;
                if terminal_for_other > 0 {
                    tx.commit()?;
                    return Ok(None);
                }
            }
            // r19 item 4: evidence objects are IMMUTABLE and
            // CONTENT-ADDRESSED. A later result (even on a terminal row) gets
            // its OWN object row and never mutates this one; the job's binding
            // is only changed by a settlement transaction that verifies the
            // linked digest, so here we never delete or overwrite and never
            // reject on a different digest — the host/merge guards above remain
            // the authority for WHOSE evidence may be stored.
            let object_id = Self::gate_log_object_id(&job_id, &digest);
            // The Node delivers one verdict twice (finished event + RPC reply)
            // with identical bytes; both resolve to the same id, so the second
            // write is an idempotent no-op.
            let same: i64 = tx.query_row(
                "SELECT COUNT(*) FROM gate_log_objects
                  WHERE id = ?1 AND digest = ?2",
                params![object_id, digest],
                |row| row.get(0),
            )?;
            if same == 0 {
                // Test-only fault: make the immutable insert fail inside the
                // transaction. There is no delete to roll back any more, so no
                // prior object can be lost; the settlement then simply has no
                // object to bind and stands down.
                #[cfg(any(test, feature = "test-faults"))]
                if insert_fault {
                    // Missing the NOT NULL job_id/project_id columns: a
                    // deterministic constraint failure.
                    tx.execute(
                        "INSERT INTO gate_log_objects (id) VALUES (?1)",
                        params![object_id],
                    )?;
                }
                tx.execute(
                    "INSERT INTO gate_log_objects
                        (id, job_id, project_id, bytes, byte_len, digest,
                         created_by, created_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        object_id,
                        job_id,
                        project_id,
                        bytes,
                        i64::try_from(bytes.len()).unwrap_or(i64::MAX),
                        digest,
                        created_by,
                        now,
                        expires_at,
                    ],
                )?;
            }
            tx.commit()?;
            Ok(Some(object_id))
        })
        .await
    }

    /// Hub-side fence for the Node's pre-execution `gate.authorize` check.
    /// Authorized only while the row is Running at EXACTLY the requested
    /// generation, assigned to the asking host, AND bound to the same
    /// authenticated carrier session the dispatch was sent on.
    pub(crate) async fn gate_dispatch_authorized(
        &self,
        job_id: &str,
        host_id: &str,
        generation: u32,
        session: u64,
    ) -> Result<GateDispatchAuth, crate::store::StoreError> {
        let job_id = job_id.to_owned();
        let host_id = host_id.to_owned();
        self.run_named("gate_dispatch_authorized", move |conn| {
            // Compare against the persisted doc directly: Running at exactly
            // this generation, this host, and the recorded carrier session.
            let authorized: i64 = conn.query_row(
                "SELECT COUNT(*) FROM gate_jobs
                  WHERE id = ?1
                    AND json_extract(doc_json, '$.state') = 'running'
                    AND json_extract(doc_json, '$.attempts') = ?2
                    AND json_extract(doc_json, '$.hostId') = ?3
                    AND dispatch_session IS NOT NULL
                    AND dispatch_session = ?4",
                params![
                    job_id,
                    generation,
                    host_id,
                    i64::try_from(session).unwrap_or(0)
                ],
                |row| row.get(0),
            )?;
            let state: Option<String> = conn
                .query_row(
                    "SELECT json_extract(doc_json, '$.state') FROM gate_jobs WHERE id = ?1",
                    params![job_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(GateDispatchAuth {
                authorized: authorized > 0,
                state,
            })
        })
        .await
    }

    /// r12 item 2: stamp the authenticated carrier session a dispatch rode,
    /// before the gate.run frame is sent. Hub-only column.
    pub(crate) async fn set_gate_dispatch_session(
        &self,
        job_id: &str,
        session: u64,
    ) -> Result<(), crate::store::StoreError> {
        let job_id = job_id.to_owned();
        self.run_named("set_gate_dispatch_session", move |conn| {
            conn.execute(
                "UPDATE gate_jobs SET dispatch_session = ?2 WHERE id = ?1",
                params![job_id, i64::try_from(session).unwrap_or(0)],
            )?;
            Ok(())
        })
        .await
    }

    /// r13 item 3: when a carrier session ends, requeue the host's `running`
    /// jobs (and finish its `canceling` jobs) that were dispatched on EXACTLY
    /// that session. One writer job; `landing` rows are excluded by the
    /// caller's occupancy guarantee. Returns the recovered rows for journaling.
    pub(crate) async fn recover_ended_gate_jobs(
        &self,
        host_id: &str,
        ended_session: u64,
    ) -> Result<Vec<GateJob>, crate::store::StoreError> {
        let host_id = host_id.to_owned();
        self.run_named("recover_ended_gate_jobs", move |conn| {
            let mut recovered = Vec::new();
            let stale: Vec<(String, String)> = {
                let mut stmt = conn.prepare(
                    "SELECT id, doc_json FROM gate_jobs
                      WHERE json_extract(doc_json, '$.hostId') = ?1
                        AND json_extract(doc_json, '$.state') IN ('running','canceling')
                        AND dispatch_session IS NOT NULL
                        AND dispatch_session = ?2",
                )?;
                let rows = stmt.query_map(
                    params![host_id, i64::try_from(ended_session).unwrap_or(0)],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (id, text) in stale {
                let Ok(mut job) = serde_json::from_str::<GateJob>(&text) else {
                    continue;
                };
                let now = now();
                let cancelling = job.state == GateJobState::Canceling;
                if cancelling {
                    job.state = GateJobState::Canceled;
                    job.finished_at = Some(now_ts());
                    if job.error.is_none() {
                        job.error = Some(
                            "carrier changed before the run registered; finished canceled".into(),
                        );
                    }
                    job.reason = Some("canceled; carrier session changed".into());
                } else {
                    job.state = GateJobState::Queued;
                    job.started_at = None;
                    job.host_id = None;
                    job.lane_id = None;
                    job.error = Some("carrier session changed while running; re-queued".into());
                }
                let doc = serde_json::to_string(&job)?;
                conn.execute(
                    "UPDATE gate_jobs
                        SET doc_json = ?1, state = ?2, lane_id = NULL,
                            dispatch_session = NULL, revision = revision + 1, updated_at = ?3
                      WHERE id = ?4",
                    params![doc, job.state.as_str(), now, id],
                )?;
                recovered.push(job);
            }
            Ok(recovered)
        })
        .await
    }

    /// r14 R2: apply a read-only landing probe verdict (`landed` or
    /// `absent`) inside ONE immediate transaction, and only when the row is
    /// still the live granted attempt the probe ran against: state Landing,
    /// `attempts` and `mergeSha` equal the snapshot, and the Hub-only
    /// `dispatch_session` equals the granted carrier session. A newer attempt
    /// (re-queued and re-granted while the probe was in flight) makes the
    /// predicate match nothing and the stale probe stands down.
    pub(crate) async fn settle_landing_probe(
        &self,
        snapshot: &GateJob,
        expected_session: Option<i64>,
        expected_host: &str,
        landed: bool,
    ) -> Result<Option<GateJob>, crate::store::StoreError> {
        let job_id = snapshot.id.as_id().to_string();
        let expected_attempts = snapshot.attempts;
        let expected_merge = snapshot.merge_sha.clone().unwrap_or_default();
        let expected_host = expected_host.to_owned();
        self.run_named("settle_landing_probe", move |conn| {
            let tx = crate::store::immediate_tx(conn)?;
            let current_state: Option<String> = tx
                .query_row(
                    "SELECT json_extract(doc_json,'$.state') FROM gate_jobs WHERE id = ?1",
                    params![job_id],
                    |row| row.get(0),
                )
                .optional()?;
            if current_state.as_deref() != Some(GateJobState::Landing.as_str()) {
                tx.commit()?;
                return Ok(None);
            }
            // Identity of the granted attempt, checked INSIDE the writer job:
            // host + attempts + merge from the doc, the bound session from its
            // column. r15 item 3 adds the authenticated responder host: a probe
            // can only settle a row still dispatched to the host that answered.
            let same_attempt: i64 = tx.query_row(
                "SELECT COUNT(*) FROM gate_jobs
                  WHERE id = ?1
                    AND json_extract(doc_json,'$.state') = 'landing'
                    AND json_extract(doc_json,'$.attempts') = ?2
                    AND json_extract(doc_json,'$.mergeSha') = ?3
                    AND json_extract(doc_json,'$.hostId') = ?5
                    AND dispatch_session IS ?4",
                params![
                    job_id,
                    expected_attempts,
                    expected_merge,
                    expected_session,
                    expected_host
                ],
                |row| row.get(0),
            )?;
            if same_attempt == 0 {
                // The probe is for an older attempt; nothing moves.
                tx.commit()?;
                return Ok(None);
            }
            let Some(mut job) = tx
                .query_row(
                    "SELECT doc_json FROM gate_jobs WHERE id = ?1",
                    params![job_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .and_then(|text| serde_json::from_str::<GateJob>(&text).ok())
            else {
                tx.commit()?;
                return Ok(None);
            };
            let now = now();
            if landed {
                job.home_land_in_flight = false;
                job.landing_session_epoch = None;
                job.landing_outcome_unknown = false;
                job.finished_at = Some(now_ts());
                job.state = GateJobState::Landed;
                job.error = None;
                if job.reason.is_none() {
                    job.reason = Some("landed; confirmed on remote main by reconciliation".into());
                }
            } else if job.attempts < MAX_LAND_ATTEMPTS {
                job.home_land_in_flight = false;
                job.landing_session_epoch = None;
                job.landing_outcome_unknown = false;
                job.state = GateJobState::Queued;
                job.started_at = None;
                job.host_id = None;
                job.lane_id = None;
                job.finished_at = None;
                job.current_main_sha = None;
                job.merge_ref = None;
                job.merge_sha = None;
                job.failed_step = None;
                job.reason = None;
                job.log_object_id = None;
                job.error = Some(
                    "landing provably absent on the remote and the land process group is verified \
                     dead; re-queued for a fresh attempt"
                        .into(),
                );
            } else {
                job.home_land_in_flight = false;
                job.landing_session_epoch = None;
                job.landing_outcome_unknown = false;
                job.state = GateJobState::Failed;
                job.finished_at = Some(now_ts());
                job.error = Some(
                    "landing provably absent on the remote with the land process group verified \
                     dead; the attempt cap is reached, so the job is not re-run automatically"
                        .into(),
                );
                job.reason =
                    Some("failed; landing provably absent and the land group is dead".into());
            }
            let doc = serde_json::to_string(&job)?;
            tx.execute(
                "UPDATE gate_jobs
                    SET doc_json = ?1, state = ?2, landing_at = NULL,
                        dispatch_session = NULL, revision = revision + 1, updated_at = ?3
                  WHERE id = ?4",
                params![doc, job.state.as_str(), now, job_id],
            )?;
            tx.commit()?;
            Ok(Some(job))
        })
        .await
    }

    /// Running → Landing transition at the verify→land boundary. Serialized
    /// in the same writer path as cancel, so a cancel committing first wins:
    /// the grant is refused when the row is no longer Running at exactly this
    /// generation/host/carrier-session. Idempotent: a repeat grant for an
    /// already-Landing row (same generation/host/session) returns granted=true;
    /// a regranted attempt riding a replacement session after a reconnect is
    /// refused even when its generation still matches.
    pub(crate) async fn gate_land_grant(
        &self,
        job_id: &str,
        host_id: &str,
        generation: u32,
        session_epoch: &str,
        merge_sha: &str,
        session: u64,
    ) -> Result<GateLandGrant, crate::store::StoreError> {
        let job_id = job_id.to_owned();
        let host_id = host_id.to_owned();
        let session_epoch = session_epoch.to_owned();
        let merge_sha = merge_sha.to_owned();
        // r17 item 7: the grant session is persisted in a positive signed i64;
        // a value that does not fit (epoch >= 2^31) is refused rather than
        // silently collapsed to 0 (which would alias the NULL/session-0 check).
        let session_i64 = i64::try_from(session).map_err(|_| {
            crate::store::StoreError::GateEpochRegressed(format!(
                "gate land grant session {session} does not fit a positive signed i64 (boot epoch \
                 at/above 2^31?); refusing"
            ))
        })?;
        self.run_named("gate_land_grant", move |conn| {
            let now = now();
            // Current state (JSON-path read, no serde error conversion).
            let state: Option<String> = conn
                .query_row(
                    "SELECT json_extract(doc_json, '$.state') FROM gate_jobs WHERE id = ?1",
                    params![job_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(state) = state else {
                return Ok(GateLandGrant {
                    granted: false,
                    state: Some("missing".into()),
                    landing_in_progress: false,
                });
            };
            // Already landing at this generation: idempotent re-answer, but
            // ONLY for the same host, the same authenticated carrier
            // session the dispatch was bound to, AND the exact GRANTED MERGE
            // (r16 item 5). A paused/old frame delivered to a replacement
            // session (generation still equal) or a re-grant carrying a
            // different verified merge is refused.
            if state == GateJobState::Landing.as_str() {
                let same_tuple: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM gate_jobs
                      WHERE id = ?1
                        AND json_extract(doc_json,'$.attempts') = ?2
                        AND json_extract(doc_json,'$.hostId') = ?3
                        AND dispatch_session IS NOT NULL
                        AND dispatch_session = ?4
                        AND json_extract(doc_json,'$.mergeSha') IS NOT NULL
                        AND json_extract(doc_json,'$.mergeSha') = ?5",
                    params![job_id, generation, host_id, session_i64, merge_sha],
                    |row| row.get(0),
                )?;
                return Ok(GateLandGrant {
                    granted: same_tuple > 0,
                    state: (same_tuple == 0).then_some(state),
                    landing_in_progress: same_tuple > 0,
                });
            }
            // Conditional Running → Landing for exactly this tuple. The grant
            // also records the verified merge it authorizes, so a restart
            // during the push can later confirm that exact commit on remote.
            let updated = conn.execute(
                "UPDATE gate_jobs
                    SET doc_json = json_set(json_set(json_set(doc_json,
                            '$.state', 'landing'),
                            '$.landingSessionEpoch', ?2),
                            '$.mergeSha', ?6),
                        state = 'landing',
                        landing_at = ?8,
                        landing_probed_at = NULL,
                        revision = revision + 1,
                        updated_at = ?3
                  WHERE id = ?1
                    AND json_extract(doc_json,'$.state') = 'running'
                    AND json_extract(doc_json,'$.attempts') = ?4
                    AND json_extract(doc_json,'$.hostId') = ?5
                    AND dispatch_session IS NOT NULL
                    AND dispatch_session = ?7",
                params![
                    job_id,
                    session_epoch,
                    now,
                    generation,
                    host_id,
                    merge_sha,
                    session_i64,
                    now,
                ],
            )?;
            if updated > 0 {
                return Ok(GateLandGrant {
                    granted: true,
                    state: None,
                    landing_in_progress: false,
                });
            }
            let state_after: String = conn.query_row(
                "SELECT json_extract(doc_json,'$.state') FROM gate_jobs WHERE id = ?1",
                params![job_id],
                |row| row.get(0),
            )?;
            Ok(GateLandGrant {
                granted: false,
                state: Some(state_after),
                landing_in_progress: false,
            })
        })
        .await
    }

    /// Load a gate log by either its `obj_…` id or the owning `gjb_…` job id.
    pub(crate) async fn get_gate_log(
        &self,
        id: &str,
    ) -> Result<Option<GateLogObject>, crate::store::StoreError> {
        let id = id.to_owned();
        self.run_named("get_gate_log", move |conn| {
            let mapper = |row: &rusqlite::Row<'_>| {
                Ok(GateLogObject {
                    object_id: row.get(0)?,
                    job_id: row.get(1)?,
                    project_id: row.get(2)?,
                    bytes: row.get(3)?,
                    expires_at: row.get(4)?,
                })
            };
            let row = if id.starts_with("obj_") {
                conn.query_row(
                    "SELECT id, job_id, project_id, bytes, expires_at
                     FROM gate_log_objects WHERE id = ?1",
                    params![id],
                    mapper,
                )
                .optional()?
            } else {
                // A job id resolves to the object the job CURRENTLY binds
                // (r19 item 4: objects are immutable and a job accumulates one
                // per verdict, so "newest row" would serve another result's
                // evidence). Pre-upgrade jobs without an obj_ reference fall
                // back to the latest row.
                let referenced: Option<String> = conn
                    .query_row(
                        "SELECT json_extract(doc_json, '$.logObjectId')
                           FROM gate_jobs WHERE id = ?1",
                        params![id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten();
                if let Some(object_id) = referenced.filter(|oid| oid.starts_with("obj_")) {
                    conn.query_row(
                        "SELECT id, job_id, project_id, bytes, expires_at
                         FROM gate_log_objects WHERE id = ?1",
                        params![object_id],
                        mapper,
                    )
                    .optional()?
                } else {
                    conn.query_row(
                        "SELECT id, job_id, project_id, bytes, expires_at
                         FROM gate_log_objects
                         WHERE job_id = ?1 ORDER BY rowid DESC LIMIT 1",
                        params![id],
                        mapper,
                    )
                    .optional()?
                }
            };
            Ok(row)
        })
        .await
    }
}

/// RFC3339 timestamp `GATE_LOG_TTL_SECS` after `now`, in the same `…Z`
/// millisecond shape `now_rfc3339` emits so lazy-expiry compares as strings.
fn gate_log_expiry(now: &str) -> String {
    let Ok(parsed) =
        time::OffsetDateTime::parse(now, &time::format_description::well_known::Rfc3339)
    else {
        return now.to_owned();
    };
    let t = parsed.to_offset(time::UtcOffset::UTC) + time::Duration::seconds(GATE_LOG_TTL_SECS);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

// ── small helpers ──────────────────────────────────────────────────────────

fn now() -> String {
    crate::config::now_rfc3339()
}

fn now_ts() -> Timestamp {
    ts(&now())
}

fn ts(value: &str) -> Timestamp {
    Timestamp::try_from(value.to_owned()).expect("now_rfc3339 is a valid timestamp")
}

async fn journal(state: &AppState, device: &str, action: &str, job: &GateJob) {
    let detail = json!({
        "projectId": job.project_id.as_id().as_str(),
        "jobId": job.id.as_id().as_str(),
        "branch": job.branch,
        "mode": job.mode.as_str(),
        "state": job.state.as_str(),
        "laneId": job.lane_id,
        "mergeSha": job.merge_sha,
        "failedStep": job.failed_step,
        "reason": job.reason,
        "logObjectId": job.log_object_id,
        "error": job.error,
    });
    if let Err(error) = state
        .store
        .append_audit(
            device.to_owned(),
            action.into(),
            Some(job.id.as_id().to_string()),
            detail,
        )
        .await
    {
        tracing::warn!(%error, action, "could not journal gate transition");
    }
}

/// Nudge the scheduler without waiting for the next tick.
fn schedule_once(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        tick(&state).await;
    });
}
