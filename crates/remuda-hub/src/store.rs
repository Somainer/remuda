//! Single-writer SQLite actor with a read-only reader pool beside it.
//!
//! Writes and short point reads run on one connection on the `remuda-hub-sqlite`
//! thread, so they serialise and never collide. Reads that can be long — a
//! journal window, an attachment blob, a list the web boot waits on — run on a
//! small pool of `SQLITE_OPEN_READ_ONLY` connections instead (hub-store-1).
//! WAL and a busy timeout already made concurrent readers legal at the SQLite
//! level; before the pool existed the Hub simply never opened a second
//! connection, so a one-row `SELECT` queued behind whatever long job was
//! already running. Connections never cross `.await`.

use crate::config::{new_id, now_rfc3339};
use crate::provider_models::{self, ProviderModel};
use remuda_protocol::SettlementOutcome;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Semaphore, oneshot};

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
    /// A uniqueness or delegation-tree invariant was violated (§2.5).
    #[error("conflict: {0}")]
    Conflict(String),
    /// A delegation scope or grant would escape the parent's authority (§2.5).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// A passkey with the same credential id is already registered.
    #[error("duplicate credential")]
    DuplicateCredential,
    /// D-057 §7.3: the commit-time initiator check refused this write — the
    /// initiator's instance was fenced or superseded, the lineage paused, or
    /// the authenticating device row no longer exists. Maps to 409 `fenced`.
    #[error("fenced")]
    Fenced,
}

fn sqlite_is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

const BUSY_WAIT: Duration = Duration::from_secs(5);

/// Read-only connections opened beside the writer. Four is enough to keep the
/// web boot's parallel list reads and a screen envelope off each other without
/// turning the page cache into a memory problem.
const READERS: usize = 4;

/// A store job slower than this gets a `warn` naming it. Evidence for the next
/// stall, not a cancellation: the job still runs to completion.
const SLOW_JOB: Duration = Duration::from_secs(1);

/// Rows one `read_journal` window may carry.
pub const JOURNAL_WINDOW_ROWS: i64 = 2_000;

/// Bytes of raw event JSON one `read_journal` window may carry. Trips before
/// [`JOURNAL_WINDOW_ROWS`] when a busy instance mirrors large payloads.
pub const JOURNAL_WINDOW_BYTES: usize = 8 * 1024 * 1024;

/// Events folded into one writer job by a multi-event `journal.append` frame.
/// 64 is roughly 100 ms of projection work and small enough that the writer
/// reaches the next queued job — a point read, another host's ingest — between
/// chunks instead of monopolising the connection for a whole 256-event page.
pub const APPEND_CHUNK_MAX: usize = 64;

/// Serialized-JSON budget for one writer job. The count cap alone lets 64
/// large transcript/screen frames become one long transaction; the byte cap
/// shrinks such a chunk so a job is bounded by volume, not just row count.
pub const APPEND_CHUNK_MAX_BYTES: usize = 1024 * 1024;

/// Split a frame's events into `[start, end)` writer-job chunks bounded by
/// [`APPEND_CHUNK_MAX`] rows and [`APPEND_CHUNK_MAX_BYTES`] of serialized
/// JSON. A single event over the byte budget still forms a chunk of one, like
/// the journal window's oversized-row rule.
pub(crate) fn journal_append_chunks(events: &[Value]) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start < events.len() {
        let mut end = start;
        let mut bytes = 0usize;
        while end < events.len() && end - start < APPEND_CHUNK_MAX {
            let size = events[end].to_string().len();
            if end != start && bytes.saturating_add(size) > APPEND_CHUNK_MAX_BYTES {
                break;
            }
            bytes = bytes.saturating_add(size);
            end += 1;
        }
        ranges.push(start..end);
        start = end;
    }
    ranges
}

/// Log a `warn` when `name` took longer than [`SLOW_JOB`].
fn note_slow(name: &'static str, kind: &'static str, elapsed: Duration) {
    if elapsed >= SLOW_JOB {
        tracing::warn!(
            job = name,
            kind,
            elapsed_ms = elapsed.as_millis() as u64,
            "hub store job exceeded budget"
        );
    }
}

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

/// Read-only connections handed out one at a time under a semaphore.
///
/// A connection is created on first use and parked back in `idle` afterwards,
/// so a Hub that never serves a long read never opens one. After
/// [`Store::close`] the pool refuses new work at both the permit gate
/// ([`Store::read`]) and [`ReaderPool::take`], so a read that lost the race
/// with shutdown cannot reopen the file behind the writer's checkpoint. A
/// connection already checked out by an in-flight read runs to completion and
/// is dropped on return rather than parked.
struct ReaderPool {
    path: PathBuf,
    permits: Semaphore,
    idle: Mutex<Vec<Connection>>,
    closed: AtomicBool,
}

impl ReaderPool {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            permits: Semaphore::new(READERS),
            idle: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }

    /// Take an idle connection, or open a fresh read-only one.
    ///
    /// Errors [`StoreError::Closed`] if the pool was retired after the caller
    /// acquired its permit.
    fn take(&self) -> Result<Connection, StoreError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(StoreError::Closed);
        }
        if let Ok(mut idle) = self.idle.lock()
            && let Some(conn) = idle.pop()
        {
            return Ok(conn);
        }
        open_reader(&self.path).map_err(StoreError::from)
    }

    /// Park a connection for reuse; a pool at capacity just drops it.
    fn put(&self, conn: Connection) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut idle) = self.idle.lock()
            && idle.len() < READERS
        {
            idle.push(conn);
        }
    }
}

/// Handle to the Hub database writer.
#[derive(Clone)]
pub struct Store {
    tx: std::sync::mpsc::Sender<Job>,
    /// Read-only connections for reads that can be long (hub-store-1).
    readers: Arc<ReaderPool>,
    /// Joins the writer thread after the last clone drops its channel sender.
    _join: Arc<StoreJoin>,
    /// Test-only seam: when armed, the NEXT `queue_command` writer job marks
    /// the named instance fenced (and bumps its lineage generation) BEFORE
    /// running `check_initiator`, deterministically reproducing a fence F that
    /// commits between request authentication and the write. Drained after
    /// one fire. Always present; production code never arms it.
    test_fence_before_queue: Arc<std::sync::Mutex<Option<String>>>,
    /// Test-only seam, device-specific companion: when armed the NEXT
    /// `queue_command` writer job deletes that device row before the authority
    /// check (the fence F also deletes predecessor devices).
    test_delete_device_before_queue: Arc<std::sync::Mutex<Option<String>>>,
    /// Test-only seam shared by gate-claim and node-op admission: arm a fence
    /// applied at the top of the NEXT such writer job.
    test_fence_before_authority: Arc<std::sync::Mutex<Option<String>>>,
    /// Test-only seam: arm a fence applied at the top of the NEXT
    /// `mutate_task` writer job only (task bind post-lease commit race).
    test_fence_before_task_mutate: Arc<std::sync::Mutex<Option<String>>>,
}

/// Delete one chapter's rows and write its interaction tombstones /
/// deleted-instance marker. Caller owns the transaction and chapter-lifecycle
/// precondition.
fn delete_instance_rows(tx: &Transaction, instance_id: &str) -> Result<(), StoreError> {
    // c-cardsettle r2 item 2: retain the terminal state of this instance's
    // interactions before their rows are deleted, so a late answer after the
    // delete gets the state-derived rejection instead of fanning out.
    tx.execute(
        "INSERT OR IGNORE INTO interaction_tombstones
            (id, instance_id, host_id, state, reason, created_at, updated_at)
         SELECT id, instance_id, host_id, state,
                COALESCE(
                    json_extract(payload_json, '$.payload.reasonCode'),
                    json_extract(payload_json,
                        '$.payload.entity.resolution.value.reason'),
                    json_extract(payload_json,
                        '$.payload.interaction.resolution.value.reason'),
                    'generation-ended'),
                created_at, updated_at
         FROM interactions WHERE instance_id = ?1",
        params![instance_id],
    )?;
    tx.execute(
        "DELETE FROM journal WHERE instance_id = ?1",
        params![instance_id],
    )?;
    tx.execute(
        "DELETE FROM commands WHERE instance_id = ?1",
        params![instance_id],
    )?;
    tx.execute(
        "DELETE FROM interactions WHERE instance_id = ?1",
        params![instance_id],
    )?;
    tx.execute(
        "DELETE FROM fleet_members WHERE instance_id = ?1",
        params![instance_id],
    )?;
    // t-pool: a deleted instance must not keep holding an attach lock.
    tx.execute(
        "UPDATE worktree_leases SET holder_instance_id = NULL, updated_at = ?2
         WHERE holder_instance_id = ?1",
        params![instance_id, now_rfc3339()],
    )?;
    // Tombstone: a Node command still draining keeps appending for this id;
    // ensure_instance must not recreate it.
    tx.execute(
        "INSERT OR REPLACE INTO deleted_instances (instance_id, deleted_at)
         VALUES (?1, ?2)",
        params![instance_id, now_rfc3339()],
    )?;
    tx.execute("DELETE FROM instances WHERE id = ?1", params![instance_id])?;
    Ok(())
}

/// Result of presenting a Node enroll or host token.
pub enum HostAuthOutcome {
    /// Token matched an enrolled host, or an enroll token minted a new one.
    Authenticated {
        /// Host index row.
        host: Box<HostRecord>,
        /// Fresh host token, only on first enrollment.
        node_token: Option<String>,
    },
    /// Secret did not match any host verifier or a live enroll token.
    Rejected,
}

/// Inputs for [`Store::authenticate_host`].
pub struct HostAuthRequest {
    /// Presented bearer secret: a host's node token, or a one-shot enroll token.
    pub presented: String,
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
    /// Authenticated device kind: human, bot, or agent. Unknown fails closed.
    pub kind: String,
    /// Agent credentials are bound to one instance.
    pub instance_id: Option<String>,
}

/// A registered WebAuthn credential (D-030). `public_key` is the
/// webauthn-rs-core `Credential` JSON; the other columns denormalize the fields
/// that list views and the clone-detection counter need without a parse.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyRecord {
    /// `cred_…`.
    pub id: String,
    /// Unpadded base64url of the WebAuthn credential id (unique).
    pub credential_id: String,
    /// webauthn-rs-core `Credential` JSON.
    pub public_key: String,
    /// Stored signature counter for clone detection.
    pub counter: i64,
    /// JSON array of reported authenticator transports.
    pub transports: String,
    /// Human label.
    pub name: String,
    /// Authenticator attestation GUID when attestation exposed one.
    pub aaguid: Option<String>,
    /// Device that registered the key (drives the "this device" hint).
    pub created_by: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

impl PasskeyRecord {
    /// rusqlite row mapper matching the explicit SELECT column lists above.
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(PasskeyRecord {
            id: row.get(0)?,
            credential_id: row.get(1)?,
            public_key: row.get(2)?,
            counter: row.get(3)?,
            transports: row.get(4)?,
            name: row.get(5)?,
            aaguid: row.get(6)?,
            created_by: row.get(7)?,
            created_at: row.get(8)?,
            last_used_at: row.get(9)?,
        })
    }
}

/// Per-host launch defaults an operator PATCH may change.
///
/// Two levels of Option per field: the outer is "did the PATCH mention this",
/// the inner is "set it or clear it". Collapsing them would leave no way to
/// remove a default once set.
#[derive(Debug, Clone, Default)]
pub struct HostLaunchDefaultsPatch {
    /// Per-host default extra CLI args. `Some(None)` clears the default.
    pub default_launch_args: Option<Option<Vec<String>>>,
    /// Per-host default claude executable. `Some(None)` clears it.
    pub claude_binary_path: Option<Option<String>>,
    /// Per-host renderer preference. `Some(None)` restores fullscreen.
    pub default_tui: Option<Option<remuda_protocol::TuiMode>>,
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
    /// Host operating system (`std::env::consts::OS`: `macos` / `linux` / …);
    /// D-045 capability preflight input. Absent = Node has not reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_os: Option<String>,
    /// Hub-supervised SSH target and binary policy, absent for externally enrolled Nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh: Option<Value>,
    /// Latest SSH preflight/connection error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Provider binding: `auto` | `native` | `profile:<id>` (D-021).
    #[serde(default = "default_provider_binding")]
    pub provider_binding: String,
    /// Per-host default extra CLI args, used when a create omits `args`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_launch_args: Option<Vec<String>>,
    /// Per-host default claude executable. Stored as given; the Node validates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_binary_path: Option<String>,
    /// Per-host requested renderer; absent means fullscreen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tui: Option<remuda_protocol::TuiMode>,
    /// Last acknowledged Node workspace registry.
    #[serde(default)]
    pub workspaces: Vec<Value>,
    /// Monotonic revision of the acknowledged workspace registry.
    #[serde(default)]
    pub workspace_revision: u64,
    /// Operator-configured relay bind for direct-network API routing
    /// (D-047 Amendment A1). Absent keeps this host's relay on loopback, so
    /// every `via` session targeting it takes `hub-relay`. Set through
    /// `PATCH /v1/hosts/{id}`, never from a Node hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_bind: Option<remuda_protocol::HostRelayBind>,
}

fn default_provider_binding() -> String {
    "auto".into()
}

fn default_provider_scope() -> String {
    "universal".into()
}

/// One staged attachment (D-027). Bytes live in the same row; the MVP keeps
/// them in SQLite rather than adding a second storage path to operate.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ObjectRecord {
    /// `obj_…`.
    pub object_id: String,
    /// Instance this attachment is staged for; also the read-authorization key.
    pub instance_id: String,
    /// Host of that instance, so a Node can only read its own attachments.
    pub host_id: String,
    /// Sniffed media type; the caller's `Content-Type` never overrides an
    /// image's sniffed type.
    pub media_type: String,
    /// Derived `<obj_id>.<ext>` name used internally for the blob.
    pub stored_name: String,
    /// Sanitised original filename supplied at upload (D-027b); the Node
    /// lands the pull under this name, with a numeric collision suffix.
    pub original_name: Option<String>,
    /// `image` vs `file` (D-027b).
    pub kind: String,
    /// Lowercase hex SHA-256 of the bytes.
    pub digest: String,
    /// Stored length.
    pub byte_len: i64,
    /// RFC3339 expiry; a row at or past it reads as absent.
    pub expires_at: String,
    /// 1-based `[Image #n]` anchor from the send that consumed this object;
    /// unset until a send manifest names it (2026-09-15).
    pub anchor: Option<i64>,
}

/// Arguments for [`Store::insert_object`].
pub struct NewObject {
    /// Target instance.
    pub instance_id: String,
    /// Host owning that instance.
    pub host_id: String,
    /// Sniffed/accepted media type.
    pub media_type: String,
    /// Extension for the derived internal blob name.
    pub extension: String,
    /// Sanitised original filename to echo back on refs (D-027b); `None` when
    /// the upload carried none.
    pub original_name: Option<String>,
    /// Lowercase hex SHA-256.
    pub digest: String,
    /// Attachment bytes.
    pub bytes: Vec<u8>,
    /// Uploading device, for the audit line.
    pub device_id: String,
    /// Staging lifetime in seconds.
    pub ttl_seconds: i64,
    /// Per-instance byte ceiling.
    pub instance_budget: i64,
}

/// Instance index row.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRecord {
    /// `ins_…`.
    pub instance_id: String,
    /// Immutable creator instance, recorded by the Hub at create time.
    pub parent_instance_id: Option<String>,
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
    /// How the create path chose native vs a Hub profile (D-021).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_source: Option<String>,
    /// Operator-facing source line (`将使用 …` / `使用主机原生登录`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_source_hint: Option<String>,
    /// Model-API route this instance actually uses (D-047). Absent on a direct
    /// session, so an existing instance's JSON is unchanged.
    ///
    /// Set from the **Node's** create result, never from the request, so the
    /// Session strip and `remuda watch` report what ran (D-035). The Hub
    /// validates the echo against the requested route before projecting it: a
    /// `via` request echoed as `direct`, or an echo naming a different proxy
    /// host, fails the create rather than recording a silent reroute. It is
    /// deliberately **not** read off the spec, because the spec carries the
    /// *requested* route — whose `route` may be `auto`, a value this
    /// observation type does not have, since a resolved route is never `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_route: Option<remuda_protocol::ApiRoute>,
    /// Current model id from create / `instance.configure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Requested harness permission mode from the create / configure spec
    /// (D-057 §3.3). The transcript-observed effective mode is
    /// `permissionEffective` inside the spec; this is the seated value that
    /// survives a continuation resume.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "permissionMode"
    )]
    pub permission_mode: Option<String>,
    /// Requested launch renderer; actual mode comes from the tty snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui: Option<remuda_protocol::TuiMode>,
    /// Native effort tier name.
    ///
    /// Normalized to a D-028 §9.1 level (`low` … `max`) on read, so a row
    /// stored with a legacy tier (`think`, `think-hard`, `default`) reads back
    /// as the level it means. `ultracode` reads back as `xhigh` with
    /// [`InstanceRecord::effort_ultracode`] set.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "effortName"
    )]
    pub effort_name: Option<String>,
    /// Dynamic-workflow flag; D-028 §9.1. Session-only, never a sixth level.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "effortUltracode"
    )]
    pub effort_ultracode: Option<bool>,
    /// Native effort tier index.
    ///
    /// Legacy field, preserved so an older client's list row still renders.
    /// It is **not** used to derive the tier: see the normalization note on
    /// `effort_name`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "effortIndex"
    )]
    pub effort_index: Option<u32>,
    /// §9.1 effective effort read back from assistant transcript records.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "effortEffective"
    )]
    pub effort_effective: Option<Value>,
    /// §9.1 effective model read back from the `/model` verdict / assistant
    /// records.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "modelEffective"
    )]
    pub model_effective: Option<Value>,
    /// §9.1 discovered session model list (gateway cache / settings / builtin).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "modelCatalog"
    )]
    pub model_catalog: Option<Value>,
    /// model-pin-1 §5.4: launch model-pin divergences projected from the
    /// Node's `model_pin_mismatch` diagnostics, verbatim. Durable across the
    /// bounded journal tail; the browser renders these in run details.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "modelPinMismatches"
    )]
    pub model_pin_mismatches: Option<Value>,
    /// Native session id reported by the driver, resumable with `--resume` (D-026).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Native transcript path, when a driver reported one (D-026).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_transcript_path: Option<String>,
    /// Signal tier confirmed by the Node for the current native session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_tier: Option<String>,
    /// Exited instance whose conversation this one continues (D-026).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    /// Journal id (`obj_…`).
    pub journal_id: String,
    /// Durable seq as decimal string.
    pub durable_seq: String,
    /// How many Hub-side `instance.configure` spec merges this instance has
    /// committed (D-055 round 2, item 4). The authoritative merge count for
    /// concurrency tests: response/Node-frame counts cannot see a merge that
    /// raced ahead of a failed or replayed forward. Starts at 0 for create.
    #[serde(default)]
    pub configure_seq: i64,
    /// Create-time.
    pub created_at: String,
    /// Update-time.
    pub updated_at: String,
    /// Last native/driver error when lifecycle is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// `native` or `promoted` — how this instance reached its `kind` (D-025).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// When a terminal was promoted to an agent.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "promotedAt"
    )]
    pub promoted_at: Option<String>,
    /// `remuda` or `user` — who ran the launch command (D-028 §1.0 rule 4).
    ///
    /// Written on the first promotion from what the row was *before* it: a
    /// `terminal` that becomes an agent is a human typing into a shell,
    /// anything else is the launch Remuda ran. Settled once, so a later
    /// demote/repromote cycle cannot rewrite a session's origin.
    ///
    /// Stored rather than inferred because §1.0 rule 2 makes promotion the only
    /// detection path — both launches promote, so `mode == promoted` stopped
    /// meaning "a human typed it" and read every Remuda-launched agent as
    /// `user` (measured in native-pty-2 §6). Rows written before the column
    /// existed still fall back to that derivation, which was sound for them.
    /// Provenance only: it never gates a capability.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "launchedBy"
    )]
    pub launched_by: Option<String>,
    /// Delegation preset name applied at create (`worker` /
    /// `project-coordinator` / `top-coordinator`); design §2.5.
    ///
    /// Display only: enforcement reads [`InstanceRecord::scope`] and
    /// [`InstanceRecord::grants`], never this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Resource reach; narrows monotonically down the delegation tree.
    #[serde(default)]
    pub scope: remuda_protocol::InstanceScope,
    /// Convenience projection of `scope.projectIds` when it has one entry.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "projectId")]
    pub project_id: Option<String>,
    /// Granted verb wire names (`dispatch` / `land` / `spend` /
    /// `address-owner`); empty = leaf worker; §2.5.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<String>,
    /// Task this node works (`tsk_…`).
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "taskId")]
    pub task_id: Option<String>,
    /// Additive per-session token/context rollup (context-usage-1). Not a
    /// column: computed from the durable `usage_events` table on every read,
    /// so TPM windows stay fresh. Absent (`null`) for sessions with no usage
    /// observations or for rows constructed outside [`load_instance`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "usageRollup"
    )]
    pub usage_rollup: Option<crate::usage_store::InstanceUsageRollup>,
    /// D-057 §5: lineage this chapter belongs to (its own id for a plain
    /// instance); serialized as `lineageId`.
    #[serde(rename = "lineageId")]
    pub lineage_id: String,
    /// Chapter position inside the lineage, 1-based.
    pub generation: i64,
    /// Why this chapter exists: `owner-resume` / later C1 causes; `null` for
    /// a first chapter or a plain instance.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "chapterCause"
    )]
    pub chapter_cause: Option<String>,
    /// When authority moved away from this chapter to its successor.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "fencedAt")]
    pub fenced_at: Option<String>,
    /// C1 restart policy copied to every chapter of a continuity lineage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<Value>,
}

/// C1 restart policy chosen at create time (D-057 §6.1).
///
/// Stored on every chapter of a continuity lineage and on the lineage row.
/// Only a Human creator may set it; no behaviour follows until `ma-restart`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartPolicy {
    /// Restart on Node-attested process loss / start failure.
    pub on_process_loss: bool,
    /// At-most restart decisions per rolling hour (>= 1).
    pub max_per_hour: u32,
}

/// One agent across process lifetimes (D-057 §5). The row exists only for
/// continuity instances: one that holds a grant or carries a restart policy.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineageRecord {
    /// First chapter's instance id.
    pub lineage_id: String,
    /// The chapter authority currently belongs to.
    pub current_instance_id: String,
    /// Current chapter generation; bumped inside every fence transaction.
    pub generation: i64,
    /// Stored state (`starting` / `running` / `paused`). Reads derive the
    /// host-offline/live view from the current chapter on top of this.
    pub state: String,
    /// Who paused (`device` / `self` / `ancestor` / `restart-cap` /
    /// `process-exit`) with its attributes; `None` while unpaused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_by: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,
    /// C1 restart policy copied to every chapter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<Value>,
    /// Reference to the first chapter's create spec and launch origin.
    #[serde(skip_serializing)]
    pub origin_spec_ref: Option<Value>,
    pub updated_at: String,
}

/// One chapter row in a lineage projection (`GET /v1/lineages/{id}`).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineageChapter {
    pub instance_id: String,
    pub generation: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chapter_cause: Option<String>,
    pub created_at: String,
    /// Timestamp of the chapter's PROCESS-END evidence (the observedAt of the
    /// classified end event / a by-construction scheduler end), stamped once
    /// and immutable. NULL while the chapter is live or when no end evidence
    /// has been recorded (host loss and ambiguous legacy failures leave it
    /// NULL — the process may still be running).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// When authority moved to the successor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fenced_at: Option<String>,
}

/// One chapter [`Store::deletion_plan`] reports: what a delete would remove,
/// where its Node data lives, the lifecycle the pre-check must gate on, and
/// the task whose worktree leases the handler must return.
#[derive(Clone, Debug)]
pub struct DeleteChapter {
    pub instance_id: String,
    pub host_id: String,
    pub lifecycle: String,
    pub task_id: Option<String>,
}

/// The outcome of the HTTP delete handler's side-effect-free pre-check
/// (ma-lineage r7 items 1-2).
#[derive(Clone, Debug)]
pub enum DeletionScope {
    /// A plain instance with no lineage row: the sole deletable row.
    Plain(DeleteChapter),
    /// A closed predecessor chapter: never deletable; its successors resolve
    /// ownership through the row.
    NonCurrent,
    /// A lineage's CURRENT chapter: deleting it removes EVERY chapter listed
    /// (plus the lineage row) in one transaction.
    Current(Vec<DeleteChapter>),
}

/// Raw `lineages` columns, in SELECT order.
type LineageRow = (
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

fn load_lineage(conn: &Connection, lineage_id: &str) -> Result<Option<LineageRecord>, StoreError> {
    let row: Option<LineageRow> = conn
        .query_row(
            "SELECT lineage_id, current_instance_id, generation, state,
                    paused_by_json, paused_at, restart_json, origin_spec_ref, updated_at
             FROM lineages WHERE lineage_id = ?1",
            params![lineage_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    let parse = |raw: Option<String>| raw.and_then(|text| serde_json::from_str(&text).ok());
    Ok(Some(LineageRecord {
        lineage_id: row.0,
        current_instance_id: row.1,
        generation: row.2,
        state: row.3,
        paused_by: parse(row.4),
        paused_at: row.5,
        restart: parse(row.6),
        origin_spec_ref: parse(row.7),
        updated_at: row.8,
    }))
}

/// Delegation-tree state attached at instance create; design §2.5.
#[derive(Debug, Clone)]
pub struct InstanceDelegation {
    /// Preset name stored for display (`worker` / `project-coordinator` /
    /// `top-coordinator`); never read by enforcement.
    pub role: Option<String>,
    /// Resource reach.
    pub scope: remuda_protocol::InstanceScope,
    /// Granted verb wire names, canonical (sorted, de-duplicated).
    pub grants: Vec<String>,
    /// Bound task (`tsk_…`).
    pub task_id: Option<String>,
    /// Whether the writer validates the §2.5 tree invariants for this insert.
    ///
    /// `true` for every `/v1/instances` create (delegation); `false` for
    /// non-delegating insert paths (fleet fan-out, D-026 resume, SSH host
    /// creates), which the operator already authorized directly.
    pub enforce_tree: bool,
    /// D-057 §6.1 C1 restart policy; `Some` makes the new instance a
    /// continuity lineage even when it holds no grants.
    pub restart: Option<RestartPolicy>,
}

impl Default for InstanceDelegation {
    /// Human-launched sessions before §2.5 read as leaf workers: a display
    /// preset, universe reach, and no verbs. Scope alone never grants an
    /// action — the grant set is what the agent-route checks read. This
    /// default bypasses tree validation because its non-create call sites
    /// (fleet/resume/SSH) are already operator-authorized.
    fn default() -> Self {
        Self {
            role: Some(remuda_protocol::ROLE_WORKER.into()),
            scope: remuda_protocol::InstanceScope::default(),
            grants: Vec::new(),
            task_id: None,
            enforce_tree: false,
            restart: None,
        }
    }
}

/// A row is "live enough to occupy a seat / fan-out slot" unless it carries
/// DEFINITE process-end evidence — the SQL mirror of
/// [`lifecycle_has_process_end_evidence`] (ma-lineage r5 item 7).
///
/// Occupies:
/// * any non-terminal lifecycle (requested/starting/running/…);
/// * an ambiguous terminal row with no end evidence — an `exited` host-lost
///   contact-loss row, or a legacy `failed` row a turn/configure error marked
///   while the process was alive. Such a row is potentially live and must keep
///   holding its seat/fan-out slot so a second live credential/child cannot
///   appear; continuation closes the process before taking the slot.
const SEAT_OCCUPIED_SQL: &str = "(
        lifecycle NOT IN ('exited', 'failed', 'closed')
        OR (lifecycle = 'exited' AND ended_at IS NULL
            AND COALESCE(last_error, '') = 'host-lost')
        OR (lifecycle = 'failed' AND ended_at IS NULL
            AND LOWER(COALESCE(last_error, '')) <> 'create-never-acknowledged'
            AND LOWER(COALESCE(last_error, '')) NOT LIKE '%start-fail%'
            AND LOWER(COALESCE(last_error, '')) NOT LIKE '%start failed%'
            AND LOWER(COALESCE(last_error, '')) NOT LIKE '%never started%')
    )";

/// Enforce the two §2.5 seat rules inside the writer thread.
///
/// 1. at most one active `address-owner` holder per Hub;
/// 2. at most one active `dispatch` holder per scoped project, unless that
///    project's policy explicitly relaxes it.
fn enforce_grant_uniqueness(
    conn: &Connection,
    delegation: &InstanceDelegation,
) -> Result<(), StoreError> {
    if !delegation.grants.contains(&"address-owner".to_string())
        && !delegation.grants.contains(&"dispatch".to_string())
    {
        return Ok(());
    }
    if delegation.grants.contains(&"address-owner".to_string()) {
        let exists: bool = conn
            .query_row(
                &format!(
                    "SELECT 1 FROM instances WHERE {SEAT_OCCUPIED_SQL}
                 AND fenced_at IS NULL
                 AND grants_json LIKE '%\"address-owner\"%' LIMIT 1"
                ),
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            return Err(StoreError::Conflict(
                "an active instance already holds the address-owner grant (one per Hub)".into(),
            ));
        }
    }
    if delegation.grants.contains(&"dispatch".to_string()) {
        for project in &delegation.scope.project_ids {
            let project = project.as_id().to_string();
            if project_allows_multiple_dispatchers(conn, &project)? {
                continue;
            }
            let exists: bool = conn
                .query_row(
                    &format!(
                        "SELECT 1 FROM instances WHERE {SEAT_OCCUPIED_SQL}
                     AND fenced_at IS NULL
                     AND grants_json LIKE '%\"dispatch\"%'
                     AND scope_json LIKE ?1 LIMIT 1"
                    ),
                    params![format!("%{project}%")],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                return Err(StoreError::Conflict(format!(
                    "an active instance already holds dispatch for project {project}; \
                     set policy.configurable.allowMultipleDispatchers to relax"
                )));
            }
        }
    }
    Ok(())
}

/// Read `allowMultipleDispatchers` off a stored project doc; missing = strict.
fn project_allows_multiple_dispatchers(
    conn: &Connection,
    project_id: &str,
) -> Result<bool, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT doc_json FROM projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(false);
    };
    let doc: Value = serde_json::from_str(&raw)?;
    Ok(doc
        .pointer("/policy/configurable/allowMultipleDispatchers")
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

/// Delegation depth/fan-out limits resolved from the child's project policy,
/// falling back to the Hub defaults (design §2.5 ⑤: limits are policy, not
/// schema).
pub(crate) struct DelegationLimits {
    /// Max edges between a human-seated node and the deepest descendant.
    pub max_depth: u32,
    /// Max active children one node may delegate.
    pub fan_out: u32,
}

pub(crate) fn delegation_limits_for(
    conn: &Connection,
    scope: &remuda_protocol::InstanceScope,
) -> Result<DelegationLimits, StoreError> {
    let mut limits = DelegationLimits {
        max_depth: remuda_protocol::DEFAULT_MAX_DELEGATION_DEPTH,
        fan_out: remuda_protocol::DEFAULT_COORDINATOR_FAN_OUT,
    };
    if let Some(project) = scope.single_project_id()
        && let Some((depth, fan)) = project_limits(conn, project.as_id().as_str())?
    {
        limits.max_depth = depth;
        limits.fan_out = fan;
    }
    Ok(limits)
}

fn project_limits(conn: &Connection, project_id: &str) -> Result<Option<(u32, u32)>, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT doc_json FROM projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let doc: Value = serde_json::from_str(&raw)?;
    let depth = doc
        .pointer("/policy/configurable/maxDelegationDepth")
        .and_then(Value::as_i64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(remuda_protocol::DEFAULT_MAX_DELEGATION_DEPTH);
    let fan = doc
        .pointer("/policy/configurable/coordinatorFanOut")
        .and_then(Value::as_i64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(remuda_protocol::DEFAULT_COORDINATOR_FAN_OUT);
    Ok(Some((depth, fan)))
}

/// Validate every §2.5 invariant for creating a child under `parent_id`.
///
/// * an agent parent must hold `dispatch`;
/// * child scope ⊆ parent scope;
/// * every child grant is held by the parent;
/// * the ancestor walk must terminate (DAG);
/// * depth stays within the (project) policy limit;
/// * a parent's active children stay within its fan-out limit.
///
/// `parent_id == None` is a human-seated root node: no narrower parent, depth
/// counted from 1.
pub(crate) fn validate_child_delegation(
    conn: &Connection,
    parent_id: Option<&str>,
    scope: &remuda_protocol::InstanceScope,
    grants: &[String],
) -> Result<(), StoreError> {
    let child_depth = if let Some(pid) = parent_id {
        let parent = load_instance(conn, pid)?
            .ok_or_else(|| StoreError::Id("unknown parent instance".into()))?;
        if !parent.grants.iter().any(|grant| grant == "dispatch") {
            return Err(StoreError::Forbidden(
                "delegating a child requires the dispatch grant".into(),
            ));
        }
        if !scope.is_subset_of(&parent.scope) {
            return Err(StoreError::Forbidden(
                "child scope must be a subset of the parent instance scope".into(),
            ));
        }
        for grant in grants {
            if !parent.grants.contains(grant) {
                return Err(StoreError::Forbidden(format!(
                    "grant {grant} is not held by the parent instance"
                )));
            }
        }
        node_depth(conn, pid)? + 1
    } else {
        1
    };
    let limits = delegation_limits_for(conn, scope)?;
    if child_depth > limits.max_depth {
        return Err(StoreError::Conflict(format!(
            "delegation depth {child_depth} exceeds the policy limit {}",
            limits.max_depth
        )));
    }
    if let Some(pid) = parent_id {
        // D-057 §5: one shared lineage resolver backs every parent edge —
        // `owns()`, the D-051 routed-decision edge and this fan-out count — so
        // they can never disagree. Children are counted per child LINEAGE
        // across every chapter: a worker that itself continued (predecessor
        // ended, successor live) still occupies one slot, and a continuation
        // chapter of the parent's own lineage never counts as a child.
        let active_children = count_active_lineage_children(conn, pid)?;
        if active_children >= limits.fan_out {
            return Err(StoreError::Conflict(format!(
                "parent {pid} already has {active_children} active children; fan-out limit is {}",
                limits.fan_out
            )));
        }
    }
    Ok(())
}

/// Edges between `id` and its human-seated root; a directly-seated node is 1.
fn node_depth(conn: &Connection, start: &str) -> Result<u32, StoreError> {
    let mut current = start.to_string();
    let mut depth = 1u32;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            return Err(StoreError::Conflict(
                "delegation cycle detected in the instance ancestor chain".into(),
            ));
        }
        let Some(instance) = load_instance(conn, &current)? else {
            break;
        };
        match instance.parent_instance_id {
            Some(parent) => {
                depth += 1;
                current = parent;
            }
            None => break,
        }
        if depth > 1000 {
            return Err(StoreError::Conflict(
                "delegation chain longer than 1000 edges; rejecting as a cycle".into(),
            ));
        }
    }
    Ok(depth)
}

/// D-057 §5: the stamped lineage id of an instance.
///
/// A row written before ma-lineage has no stamped `lineage_id`, and a plain
/// instance is always its own lineage, so both read as the instance's own id.
/// `None` means the instance does not exist.
pub(crate) fn lineage_id_of(conn: &Connection, id: &str) -> Result<Option<String>, StoreError> {
    conn.query_row(
        "SELECT COALESCE(lineage_id, id) FROM instances WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

/// D-057 §5: the single lineage edge every parent-edge rule reads
/// (`owns()`, the D-051 routed-decision edge, fan-out).
///
/// A caller owns a target when the target is a chapter in the caller's
/// lineage, or the target's parent is. Successor chapters therefore keep
/// reading and controlling what predecessor chapters created, while plain
/// instances keep the old self-or-direct-parent semantics (each plain
/// instance is its own lineage).
pub(crate) fn lineage_owns_conn(
    conn: &Connection,
    caller_id: &str,
    target_id: &str,
) -> Result<bool, StoreError> {
    let Some(caller_lineage) = lineage_id_of(conn, caller_id)? else {
        return Ok(false);
    };
    let Some(target_lineage) = lineage_id_of(conn, target_id)? else {
        return Ok(false);
    };
    if target_lineage == caller_lineage {
        return Ok(true);
    }
    Ok(parent_lineage_of(conn, target_id)? == Some(caller_lineage))
}

/// Stamped lineage id of `id`'s parent edge, when the edge names an existing
/// instance. The shared half of the lineage resolver: every parent-edge rule
/// ultimately compares this value against a caller lineage.
pub(crate) fn parent_lineage_of(conn: &Connection, id: &str) -> Result<Option<String>, StoreError> {
    let parent: Option<String> = conn
        .query_row(
            "SELECT json_extract(spec_json, '$.parentInstanceId')
             FROM instances WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    match parent {
        Some(parent) => Ok(lineage_id_of(conn, &parent)?),
        None => Ok(None),
    }
}

/// Count the parent lineage's **active child lineages** — the fan-out resolver
/// (D-057 §5), expressed with the same lineage edge [`lineage_owns_conn`] uses.
///
/// Each distinct child lineage with at least one non-terminal, non-fenced
/// chapter counts once: a worker that itself continued (its predecessor
/// exited and its successor chapter is live) still occupies one slot, which
/// the earlier per-row `chapter_cause IS NULL` count dropped. The parent's own
/// continuation chapters are excluded: a chapter of the parent lineage is not
/// a child.
pub(crate) fn count_active_lineage_children(
    conn: &Connection,
    parent_id: &str,
) -> Result<u32, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT COALESCE(child.lineage_id, child.id))
         FROM instances child
         WHERE child.fenced_at IS NULL
           AND (
                child.lifecycle NOT IN ('exited', 'failed', 'closed')
                OR (child.lifecycle = 'exited' AND child.ended_at IS NULL
                    AND COALESCE(child.last_error, '') = 'host-lost')
                OR (child.lifecycle = 'failed' AND child.ended_at IS NULL
                    AND LOWER(COALESCE(child.last_error, '')) <> 'create-never-acknowledged'
                    AND LOWER(COALESCE(child.last_error, '')) NOT LIKE '%start-fail%'
                    AND LOWER(COALESCE(child.last_error, '')) NOT LIKE '%start failed%'
                    AND LOWER(COALESCE(child.last_error, '')) NOT LIKE '%never started%')
           )
           AND COALESCE(child.lineage_id, child.id) <>
               (SELECT COALESCE(lineage_id, id) FROM instances WHERE id = ?1)
           AND EXISTS (
               SELECT 1 FROM instances edge
               WHERE COALESCE(edge.lineage_id, edge.id)
                     = COALESCE(child.lineage_id, child.id)
                 AND json_extract(edge.spec_json, '$.parentInstanceId') IS NOT NULL
                 AND (SELECT COALESCE(parent.lineage_id, parent.id)
                        FROM instances parent
                       WHERE parent.id
                             = json_extract(edge.spec_json, '$.parentInstanceId'))
                     = (SELECT COALESCE(lineage_id, id)
                          FROM instances WHERE id = ?1)
           )",
        params![parent_id],
        |row| row.get(0),
    )?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Inputs to the ma-lineage continuation-resume transaction.
pub struct ContinuationResumeRequest {
    /// Any chapter the owner addressed; the transaction fences the lineage's
    /// current chapter.
    pub addressed_instance_id: String,
    /// Generation the HTTP handler observed before queueing the writer job;
    /// the CAS fails when another resume/restart committed first.
    pub expected_generation: i64,
    /// Successor host — always the current chapter's host.
    pub host_id: String,
    /// Resolved create spec for the successor (continuation edge, resume id,
    /// provider overlay already attached by the handler).
    pub spec: Value,
    /// `instance.resume` (recovery from a native session id) or
    /// `instance.create` (fresh launch from the origin spec).
    pub operation: String,
    /// Optional first prompt for the successor.
    pub prompt: Option<String>,
    /// Wire origin of the resume create (`human` for an owner Resume).
    pub origin: String,
    /// UI title for the successor.
    pub title: Option<String>,
}

/// Payload of the winning continuation transaction.
#[derive(Debug)]
pub struct ContinuationResumed {
    pub lineage_id: String,
    pub fenced: Box<InstanceRecord>,
    pub successor: InstanceRecord,
    pub command: CommandRecord,
}

/// Outcome of [`Store::continuation_resume`].
#[derive(Debug)]
pub enum ContinuationResumeResult {
    /// This transaction fenced the predecessor and inserted the successor.
    Resumed(Box<ContinuationResumed>),
    /// The generation CAS lost: the lineage already advanced. `current` is
    /// the chapter the winning transaction created, which the caller presents
    /// as an idempotent replay instead of starting another chapter.
    Superseded { current: Box<InstanceRecord> },
}

/// Hook fired at continuation-resume points with `(point, lineageId)`.
pub(crate) type ContinuationHook = std::sync::Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Deterministic hook points for the §7.10 race suites. Production builds
/// leave the slot empty; tests install a blocking rendezvous through
/// [`lineage_test_support`].
static CONTINUATION_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<ContinuationHook>>> =
    std::sync::OnceLock::new();

/// Test-only seam for the ma-lineage continuation-resume race.
#[doc(hidden)]
pub mod lineage_test_support {
    /// Hook fired at continuation-resume points with `(point, lineageId)`.
    pub type Hook = std::sync::Arc<dyn Fn(&str, &str) + Send + Sync>;

    /// Install a hook fired at continuation-resume points with `(point,
    /// lineageId)`. `None` removes it.
    ///
    /// `read` fires in the HTTP handler just after the generation is read and
    /// before the writer job is queued; `cas` fires inside the writer
    /// transaction between the lineage read and its compare-and-set. A
    /// blocking rendezvous on `cas` serialises two resumes deterministically.
    pub fn set_hook(hook: Option<Hook>) {
        let slot = super::CONTINUATION_HOOK.get_or_init(|| std::sync::Mutex::new(None));
        *slot.lock().expect("continuation hook lock") = hook;
    }
}

/// Invoke an installed continuation-resume hook at `point` with the lineage
/// id. `read` fires in the HTTP handler after the generation read; `cas`
/// fires inside the writer transaction between the read and the CAS write.
///
/// `read` (async runtime) and `cas` (writer thread) fire concurrently during
/// the race suites, so the slot is cloned, never taken, per invocation.
pub(crate) fn run_continuation_hook(point: &str, lineage_id: &str) {
    let Some(slot) = CONTINUATION_HOOK.get() else {
        return;
    };
    let hook = slot.lock().ok().and_then(|guard| guard.clone());
    if let Some(hook) = hook {
        hook(point, lineage_id);
    }
}

/// The fence-and-continue transaction behind [`Store::continuation_resume`].
fn continuation_resume_tx(
    conn: &mut Connection,
    mut request: ContinuationResumeRequest,
) -> Result<ContinuationResumeResult, StoreError> {
    let tx = conn.transaction()?;
    let addressed = load_instance(&tx, &request.addressed_instance_id)?
        .ok_or_else(|| StoreError::Id("unknown instance".into()))?;
    let lineage = load_lineage(&tx, &addressed.lineage_id)?
        .ok_or_else(|| StoreError::Id("addressed instance is not a continuity lineage".into()))?;
    let lineage_id = lineage.lineage_id.clone();
    let current_id = lineage.current_instance_id.clone();
    run_continuation_hook("cas", &lineage_id);
    // ma-lineage round 2: the owner may address an OLDER, already-fenced
    // chapter. Such a resume is the same continuation its successor already
    // represents — return that successor idempotently instead of fencing the
    // live chapter and minting another one. Only the lineage's CURRENT chapter
    // starts another continuation. Placed after the CAS rendezvous hook (which
    // serializes concurrent resumes) but before any write: a racing loser that
    // observed the old generation rolls back here and returns the winner.
    if addressed.instance_id != current_id {
        let current = load_instance(&tx, &current_id)?
            .ok_or_else(|| StoreError::Id("current chapter missing".into()))?;
        return Ok(ContinuationResumeResult::Superseded {
            current: Box::new(current),
        });
    }
    let successor_id = new_id("ins").map_err(|e| StoreError::Id(e.to_string()))?;
    let journal_id = new_id("obj").map_err(|e| StoreError::Id(e.to_string()))?;
    let command_id = new_id("cmd").map_err(|e| StoreError::Id(e.to_string()))?;
    let now = now_rfc3339();
    let successor_generation = lineage.generation + 1;
    // Compare-and-set on (generation, current chapter): a racing resume that
    // observed the same lineage state matches zero rows.
    let cas = tx.execute(
        "UPDATE lineages
            SET current_instance_id = ?1, generation = ?2, state = 'starting',
                updated_at = ?3
          WHERE lineage_id = ?4 AND generation = ?5 AND current_instance_id = ?6",
        params![
            successor_id,
            successor_generation,
            now,
            lineage_id,
            request.expected_generation,
            current_id,
        ],
    )?;
    if cas == 0 {
        // The generation CAS lost: the lineage already advanced — possibly
        // because the addressed chapter was already fenced and the live
        // chapter moved on. Roll the (empty) transaction back and present the
        // winner's current chapter as an idempotent replay. A resume addressed
        // to the fenced chapter must therefore converge on the existing
        // successor instead of fencing the live one (ma-lineage round 2).
        drop(tx);
        let lineage = load_lineage(conn, &lineage_id)?
            .ok_or_else(|| StoreError::Id("lineage vanished after lost CAS".into()))?;
        let current = load_instance(conn, &lineage.current_instance_id)?
            .ok_or_else(|| StoreError::Id("current chapter vanished after lost CAS".into()))?;
        return Ok(ContinuationResumeResult::Superseded {
            current: Box::new(current),
        });
    }
    // 1. Fence the CURRENT chapter — which is what the generation CAS just
    // advanced from. The owner may have addressed an older fenced chapter;
    // `current_id` is the lineage's current chapter regardless, so the live
    // successor is what gets fenced and never a chapter fenced already.
    tx.execute(
        "UPDATE instances SET fenced_at = ?1, updated_at = ?2 WHERE id = ?3",
        params![now, now, current_id],
    )?;
    // 2. Delete every device row bound to the predecessor: its launch
    //    credential and any MCP token minted for it.
    tx.execute("DELETE FROM devices WHERE instance_id = ?1", [&current_id])?;
    let current = load_instance(&tx, &current_id)?
        .ok_or_else(|| StoreError::Id("current chapter missing".into()))?;
    // ma-lineage round 2: the seated permission mode is part of how the
    // successor runs. The HTTP layer builds the successor spec from
    // `spec_for_resume`, which does not carry the raw create spec, so carry
    // both the requested `permissionMode` and the transcript-observed
    // `permissionEffective` over here, filling only what the prepared spec
    // lacks. A fresh-launch recovery (origin spec) already carries the
    // requested mode, so its value wins.
    let predecessor_spec: Value = tx
        .query_row(
            "SELECT spec_json FROM instances WHERE id = ?1",
            params![current_id],
            |row| {
                let raw: String = row.get(0)?;
                Ok(serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({})))
            },
        )
        .map_err(StoreError::from)?;
    if let Some(spec) = request.spec.as_object_mut() {
        for key in ["permissionMode", "permissionEffective"] {
            let missing = spec.get(key).is_none_or(Value::is_null);
            if missing
                && let Some(value) = predecessor_spec.get(key).filter(|value| !value.is_null())
            {
                spec.insert(key.to_owned(), value.clone());
            }
        }
    }
    let host =
        load_host(&tx, &request.host_id)?.ok_or_else(|| StoreError::Id("unknown host".into()))?;
    let connectivity = if host.online {
        "connected"
    } else {
        "disconnected"
    };
    // 4. Successor: continuation edge, delegation and restart copied.
    let scope_json = serde_json::to_string(&current.scope)?;
    let grants_json = serde_json::to_string(&current.grants)?;
    let restart_json = current
        .restart
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let successor_delegation = InstanceDelegation {
        role: current.role.clone(),
        scope: current.scope.clone(),
        grants: current.grants.clone(),
        task_id: current.task_id.clone(),
        enforce_tree: false,
        restart: current
            .restart
            .clone()
            .and_then(|value| serde_json::from_value(value).ok()),
    };
    // The predecessor is fenced inside this same transaction, so the copied
    // seat grants do not collide with it.
    enforce_grant_uniqueness(&tx, &successor_delegation)?;
    tx.execute(
        "INSERT INTO instances
            (id, host_id, workspace_id, kind, driver, lifecycle, activity, connectivity,
             title, journal_id, durable_seq, spec_json, created_at, updated_at,
             role, scope_json, grants_json, task_id,
             lineage_id, generation, chapter_cause, fenced_at, restart_json)
         VALUES (?1, ?2, ?3, ?4, ?5, 'requested', 'unknown', ?6,
                 ?7, ?8, 0, ?9, ?10, ?10,
                 ?11, ?12, ?13, ?14,
                 ?15, ?16, 'owner-resume', NULL, ?17)",
        params![
            successor_id,
            current.host_id,
            current.workspace_id,
            current.kind,
            current.driver,
            connectivity,
            request.title,
            journal_id,
            request.spec.to_string(),
            now,
            current.role,
            scope_json,
            grants_json,
            current.task_id,
            lineage_id,
            successor_generation,
            restart_json,
        ],
    )?;
    // 5. Queue the successor's resume create (forwarded after commit).
    let initial_input = request
        .prompt
        .as_ref()
        .map(|text| json!({ "type": "prompt", "text": text }));
    let payload = json!({
        "origin": request.origin,
        "instanceId": successor_id,
        "spec": request.spec,
        "initialInput": initial_input,
    });
    tx.execute(
        "INSERT INTO commands
            (id, instance_id, host_id, operation, state, resolution, forwarded,
             payload_json, idempotency_key, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'queued', 'clear', 0, ?5, NULL, ?6, ?6)",
        params![
            command_id,
            successor_id,
            current.host_id,
            request.operation,
            payload.to_string(),
            now
        ],
    )?;
    let fenced = load_instance(&tx, &current_id)?
        .ok_or_else(|| StoreError::Id("fenced chapter missing".into()))?;
    let successor = load_instance(&tx, &successor_id)?
        .ok_or_else(|| StoreError::Id("successor insert missing".into()))?;
    let command = load_command(&tx, &command_id)?
        .ok_or_else(|| StoreError::Id("successor command missing".into()))?;
    tx.commit()?;
    Ok(ContinuationResumeResult::Resumed(Box::new(
        ContinuationResumed {
            lineage_id,
            fenced: Box::new(fenced),
            successor,
            command,
        },
    )))
}

impl InstanceRecord {
    ///
    /// Everything that decides *how* the native process runs is kept, so the
    /// continued conversation talks to the same provider under the same
    /// permissions; runtime observations and the child's own identity are not.
    pub fn spec_for_resume(&self) -> Value {
        let mut spec = json!({
            "kind": self.kind,
            "driver": self.driver,
            "workspaceId": self.workspace_id,
            "cwd": self.cwd,
            "model": self.model,
            "tui": self.tui,
            "delegation": self.delegation,
            "providerProfileId": self.provider_profile_id,
        });
        if let Some(object) = spec.as_object_mut() {
            if let Some(name) = &self.effort_name {
                // Write the normalized D-028 shape so the resumed child never
                // has to re-normalize, and keep the legacy keys for a Node
                // that has not picked up the new field yet.
                object.insert(
                    "effort".into(),
                    json!({ "name": name, "ultracode": self.effort_ultracode.unwrap_or(false) }),
                );
                object.insert("effortName".into(), json!(name));
            }
            if let Some(index) = self.effort_index {
                object.insert("effortIndex".into(), json!(index));
            }
            object.retain(|_, value| !value.is_null());
        }
        spec
    }
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
    /// `queued` / `accepted` / `settled` — the only three Command progress
    /// states (protocol §2.5, §12.2). A failed delivery is a *settled* command
    /// with a rejection settlement, never a fourth state.
    pub state: String,
    /// `clear` / `unknown` / `reconciling`. A dispatched send the Hub cannot
    /// resolve rests at `unknown`; it is never promoted to a fourth value.
    pub resolution: String,
    /// True after Hub persisted a forward intent (never resend).
    pub forwarded: bool,
    /// Settlement outcome once `state == settled` (`completed` / `rejected` /
    /// `cancelled`), parsed from protocol §2.5 `SettlementOutcome`. Internal
    /// ledger column: projected to the wire as `settlement.outcome`.
    #[serde(skip)]
    pub settlement_outcome: Option<String>,
    /// Human reason for a `rejected` settlement; projected as
    /// `settlement.reason`. Internal ledger column, never a top-level field.
    #[serde(skip)]
    pub settlement_reason: Option<String>,
    /// HTTP status the first attempt answered with, persisted for a
    /// non-replayable command (D-055 round 2) so a replay reproduces the
    /// original outcome instead of a do-nothing success. NULL when the first
    /// attempt answered 200 (its row IS the outcome) or for older rows.
    #[serde(skip)]
    pub settlement_http_status: Option<i64>,
    /// Exact JSON body the first attempt answered with, paired with
    /// [`Self::settlement_http_status`]. Internal ledger column.
    #[serde(skip)]
    pub settlement_http_body: Option<String>,
    /// Protocol §2.5 settlement projection, present only once settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement: Option<CommandSettlement>,
    /// D-057 §7.1: Hub-stamped initiator of an Agent-initiated command; null
    /// for Human/Bot commands. Projected to the wire and forwarded to Nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<remuda_protocol::Initiator>,
    /// D-057 §7.1: authenticating device id. Hub-only ledger column: never
    /// serialized to clients and never sent to Nodes.
    #[serde(skip)]
    pub initiator_device_id: Option<String>,
    /// Original payload.
    pub payload: Value,
    /// Optional caller idempotency key.
    pub idempotency_key: Option<String>,
    /// Create-time.
    pub created_at: String,
    /// Update-time.
    pub updated_at: String,
}

/// Hub projection of protocol §2.5 `Command.settlement`. A settled command
/// always carries an `outcome` drawn from the wire `SettlementOutcome` enum;
/// `reason` holds the Node's message for a `rejected` settlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSettlement {
    /// `completed` / `rejected` / `cancelled` (§2.5 `SettlementOutcome`).
    pub outcome: String,
    /// The Node's rejection message; present only for `rejected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CommandRecord {
    /// Build the wire settlement projection from the ledger columns.
    fn settlement_projection(
        outcome: Option<&str>,
        reason: Option<&str>,
    ) -> Option<CommandSettlement> {
        outcome.map(|outcome| CommandSettlement {
            outcome: outcome.to_owned(),
            reason: reason.filter(|_| outcome == "rejected").map(str::to_owned),
        })
    }
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

/// Default window a `requested` instance may wait for a Node receipt (ms).
///
/// Past it the row stops holding a placement slot and the sweeper fails it.
pub const REQUESTED_SLOT_WINDOW_MS: u64 = 300_000;

/// Instances holding a placement slot on a host.
///
/// Only Node-confirmed lifecycles count. `requested` is deliberately excluded:
/// it is a Hub-side intent the Node has not acknowledged, so stale ones (Node
/// restarted, create never settled) would pin a host at `maxInstances` forever
/// — the failure seen on the demo. [`Store::expire_stale_requested`] fails
/// those rows outright once their window passes.
const LIVE_INSTANCE_COUNT_SQL: &str = "SELECT COUNT(*) FROM instances
     WHERE host_id = ?1 AND lifecycle IN
        ('preparing', 'starting', 'ready', 'running', 'closing', 'reconciling')";

/// Backstop count used inside the writer thread when inserting an instance.
///
/// Same live set as [`LIVE_INSTANCE_COUNT_SQL`] plus `requested` rows younger
/// than [`REQUESTED_SLOT_WINDOW_MS`]. Placement has already passed by then;
/// this only stops a burst of concurrent creates from blowing past the cap in
/// the instant before any of them reports ready. It is bounded by age, so it
/// can never wedge a host the way the old unbounded predicate did.
const INSERT_SLOT_COUNT_SQL: &str = "SELECT COUNT(*) FROM instances
     WHERE host_id = ?1 AND (
        lifecycle IN ('preparing', 'starting', 'ready', 'running', 'closing', 'reconciling')
        OR (lifecycle = 'requested' AND
            (julianday('now') - julianday(created_at)) * 86400000 < ?2)
     )";

/// Result of [`Store::append_journal`].
#[derive(Clone, Debug)]
pub struct JournalAppend {
    /// Mirrored row (existing row when `replayed`).
    pub record: JournalRecord,
    /// True when this seq was already durable; callers must not fan out again.
    pub replayed: bool,
    /// Inclusive instance watermark after this call (may exceed `record.seq` on replay).
    pub durable_seq: i64,
    /// c-cardsettle: pending interactions this append invalidated because the
    /// event moved the instance into a terminal lifecycle. Empty on replay.
    /// Callers broadcast it AFTER the transaction committed.
    pub settlement: Settlement,
}

/// One bounded [`Store::read_journal`] window with the metadata a caller needs
/// to tell a complete page from a partial tail window (hub-store-1).
#[derive(Clone, Debug)]
pub struct JournalPage {
    /// Window events in ascending seq order, newest rows when the cap tripped.
    pub events: Vec<JournalRecord>,
    /// Durable seq read from the SAME snapshot as `events`, so every returned
    /// event has `seq <= durable_seq`.
    pub durable_seq: i64,
    /// seq of `events[0]` — the window floor — or `None` for an empty window.
    pub from_seq: Option<i64>,
    /// Whether the window reaches `after_seq`: true for an empty window or when
    /// `from_seq == after_seq + 1`. False means rows exist below the floor and
    /// the caller holds a tail window, not the whole range.
    pub reached_after_seq: bool,
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

/// Terminal state retained for an interaction after its owning instance was
/// deleted (c-cardsettle r2 item 2), so a late answer gets the state-derived
/// rejection instead of a fan-out to every connected Node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractionTombstone {
    /// `int_…`.
    pub interaction_id: String,
    /// Deleted owning instance.
    pub instance_id: String,
    /// Owning host at delete time.
    pub host_id: String,
    /// Durable interaction state at delete (`invalidated` / `expired` / …).
    pub state: String,
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
    /// Catalog models (structured; legacy string lists migrate on read).
    pub models: Vec<ProviderModel>,
    /// Prefill for New Session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Extra HTTP headers (never the auth token).
    pub headers: BTreeMap<String, String>,
    /// New Session `delegation=gateway` selects this profile.
    pub default_gateway: bool,
    /// `universal` or `host:<hostId>` (D-021).
    #[serde(default = "default_provider_scope")]
    pub scope: String,
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
    /// Declared supply + observed window state (coordinator §4.2).
    #[serde(default)]
    pub supply: remuda_protocol::SupplyProfile,
    /// How sessions using this profile reach the model API (D-047). The D2
    /// default is `{mode: direct, route: auto}`, exactly what every profile
    /// stored before D-047 reads back as.
    #[serde(default)]
    pub delivery: remuda_protocol::ProviderDelivery,
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
            "models": self.models.iter().map(ProviderModel::to_json).collect::<Vec<_>>(),
            "defaultModel": self.default_model,
            "headers": self.headers,
            "defaultGateway": self.default_gateway,
            "scope": if self.scope.is_empty() {
                "universal".to_string()
            } else {
                self.scope.clone()
            },
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
            "supply": self.supply,
            "delivery": self.delivery,
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
            .or_else(|| provider_models::enabled_ids(&self.models).first().copied())
            .unwrap_or("");
        let mut overlay = json!({
            "profileId": self.id,
            "kind": self.kind,
            "baseUrl": self.base_url,
            "model": model,
            "headers": self.headers,
            "scope": if self.scope.is_empty() {
                "universal".to_string()
            } else {
                self.scope.clone()
            },
        });
        // D-047: carry the profile delivery only when non-default, so a
        // direct/auto profile's overlay is byte-identical to the pre-D-047
        // shape. The resolved per-launch route rides `InstanceSpec.apiRoute`.
        if !self.delivery.is_direct_default()
            && let Some(obj) = overlay.as_object_mut()
            && let Ok(value) = serde_json::to_value(&self.delivery)
        {
            obj.insert("delivery".into(), value);
        }
        overlay
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

/// Outcome of a write that moved instance(s) into a terminal lifecycle
/// (c-cardsettle): one interaction invalidated IN THE SAME TRANSACTION as its
/// instance ending.
///
/// The store never depends on [`crate::AppState`]; callers publish these on
/// the follow bus so every open inbox/session drops the card immediately
/// instead of waiting for its next poll. Idempotent writes settle nothing and
/// yield an empty settlement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settlement {
    /// Interactions settled to `invalidated`, in commit order.
    pub interactions: Vec<SettledInteraction>,
}

/// One interaction a terminal transaction settled; `updated_at` is the durable
/// write timestamp the lag-recovery cursor pages on (c-cardsettle r6 item 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledInteraction {
    /// Owning instance.
    pub instance_id: String,
    /// The interaction id that was invalidated.
    pub interaction_id: String,
    /// Durable `updated_at` of the terminal row, for delivery cursoring.
    pub updated_at: String,
}

impl Settlement {
    /// No settled pairs — callers skip the broadcast.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.interactions.is_empty()
    }

    /// Fold another settlement into this one (e.g. per-event settlements of one
    /// journal chunk).
    pub fn merge(&mut self, other: Settlement) {
        self.interactions.extend(other.interactions);
    }
}

/// c-cardsettle r5 item 6: bound on one settlement lag-recovery page (live
/// rows and tombstones counted together), so a lag burst can never produce
/// an unbounded recovery frame; followers page forward from their cursor.
pub(crate) const SETTLEMENT_LAG_PAGE: u32 = 512;

/// Separator inside an opaque settlement lag cursor (`updated_at` + SEP +
/// `id`). RFC3339 timestamps and wire ids never contain the ASCII unit
/// separator, so the token round-trips unambiguously.
const SETTLEMENT_CURSOR_SEP: char = '\u{1f}';

/// One row of the `worktree_leases` table (task-model t-pool).
///
/// Identity is `(host_id, workspace_id, dir_key)`: the Space key plus the
/// directory relative to the workspace root. A reuse-to-root lease has
/// `dir_key = "."` and `worktree_name = NULL`; a pool slot carries its slot
/// name. `refcount` is the number of tasks serially sharing that one
/// directory, and `holder_instance_id` is the attach lock — `None` once every
/// session has detached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeLeaseRow {
    /// `wtl_…` row id.
    pub id: String,
    /// `reuse` | `pool`; reset/clean/park semantics are pool-only.
    pub mode: String,
    /// Owning host.
    pub host_id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Directory key relative to the workspace root; `"."` is the root.
    pub dir_key: String,
    /// Worktree/slot name; `None` for a reuse-to-root lease.
    pub worktree_name: Option<String>,
    /// Branch checked out while leased.
    pub branch: Option<String>,
    /// Project this lease serves, when known.
    pub project_id: Option<String>,
    /// Number of tasks sharing the directory.
    pub refcount: i64,
    /// `leased` | `parked` | `provisioning`.
    pub state: String,
    /// Attach-lock holder; one session at a time.
    pub holder_instance_id: Option<String>,
    /// Tasks currently counted on this lease.
    pub task_ids: Vec<String>,
    /// Oid the slot parked at / branched from.
    pub base_oid: Option<String>,
    /// Creation timestamp.
    pub created_at: String,
    /// Last state update.
    pub updated_at: String,
    /// When refcount last reached zero.
    pub released_at: Option<String>,
}

/// Whether `dir_key` is a pool slot `<pool>-s<n>` of `pool`. The reserved
/// `-s` suffix (mirrors the Node's slot naming) keeps a similarly named but
/// distinct pool from matching: pool `a` does not own slot `ab-s1`.
fn is_pool_slot_of(pool: &str, dir_key: &str) -> bool {
    match dir_key.strip_prefix(&format!("{pool}-s")) {
        Some(number) => !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

impl WorktreeLeaseRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let task_ids_json: String = row.get("task_ids_json")?;
        let task_ids: Vec<String> = serde_json::from_str(&task_ids_json).unwrap_or_default();
        Ok(Self {
            id: row.get("id")?,
            mode: row.get("mode")?,
            host_id: row.get("host_id")?,
            workspace_id: row.get("workspace_id")?,
            dir_key: row.get("dir_key")?,
            worktree_name: row.get("worktree_name")?,
            branch: row.get("branch")?,
            project_id: row.get("project_id")?,
            refcount: row.get("refcount")?,
            state: row.get("state")?,
            holder_instance_id: row.get("holder_instance_id")?,
            task_ids,
            base_oid: row.get("base_oid")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            released_at: row.get("released_at")?,
        })
    }

    pub(crate) fn load(
        conn: &Connection,
        host_id: &str,
        workspace_id: &str,
        dir_key: &str,
    ) -> rusqlite::Result<Option<Self>> {
        conn.query_row(
            "SELECT * FROM worktree_leases
             WHERE host_id = ?1 AND workspace_id = ?2 AND dir_key = ?3",
            params![host_id, workspace_id, dir_key],
            Self::from_row,
        )
        .optional()
    }

    fn load_by_id(conn: &Connection, id: &str) -> rusqlite::Result<Self> {
        conn.query_row(
            "SELECT * FROM worktree_leases WHERE id = ?1",
            params![id],
            Self::from_row,
        )
    }

    fn insert(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO worktree_leases
                (id, mode, host_id, workspace_id, dir_key, worktree_name, branch,
                 project_id, refcount, state, holder_instance_id, task_ids_json,
                 base_oid, created_at, updated_at, released_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, ?15)",
            params![
                self.id,
                self.mode,
                self.host_id,
                self.workspace_id,
                self.dir_key,
                self.worktree_name,
                self.branch,
                self.project_id,
                self.refcount,
                self.state,
                self.holder_instance_id,
                serde_json::to_string(&self.task_ids).expect("task ids"),
                self.base_oid,
                self.created_at,
                self.released_at,
            ],
        )?;
        Ok(())
    }

    fn save(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE worktree_leases SET
                mode = ?2, worktree_name = ?3, branch = ?4, project_id = ?5,
                refcount = ?6, state = ?7, holder_instance_id = ?8,
                task_ids_json = ?9, base_oid = ?10, updated_at = ?11,
                released_at = ?12
             WHERE id = ?1",
            params![
                self.id,
                self.mode,
                self.worktree_name,
                self.branch,
                self.project_id,
                self.refcount,
                self.state,
                self.holder_instance_id,
                serde_json::to_string(&self.task_ids).expect("task ids"),
                self.base_oid,
                self.updated_at,
                self.released_at,
            ],
        )?;
        Ok(())
    }
}

/// Dispatch-time classification of the current attach-lock holder.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LeaseHolder {
    /// No holder (or a reclaimable stale one): claim the empty condition.
    Free,
    /// Reclaimable holder to match conditionally (same task's session/tab or
    /// a stale instance/claim); carries its exact current value.
    Reclaimable(String),
    /// Holder belongs to another task and is live: queue this launch.
    Busy(String),
}

impl Store {
    /// Open (or create) `hub.sqlite` on a dedicated writer thread, plus a
    /// read-only pool for reads that can be long (hub-store-1).
    ///
    /// IDENTITY-KEY SITE (spec only — `docs/design/protocol.md` §7.7): this
    /// `data_dir` is also the specified home of the future Hub ed25519 identity
    /// key pair (`hub-identity/identity.ed25519[.pub]`, generated once on first
    /// start, key `0600`, directory `0700`). The private key belongs in the same
    /// backup set as `bootstrap-token` and the secret envelopes: a restore that
    /// loses it mints a new Hub identity and invalidates every Node pin. Nothing
    /// is created here yet — the spec is docs-only; no schema, code, or wire
    /// change accompanies this comment.
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(data_dir).map_err(|err| StoreError::Id(err.to_string()))?;
        let path = data_dir.join("hub.sqlite");
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        // Readers open `SQLITE_OPEN_READ_ONLY`, which cannot create the file or
        // the schema. Wait for the writer to finish `open_conn` before handing
        // back a Store, so the first read cannot race schema creation.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let writer_path = path.clone();
        let thread = thread::Builder::new()
            .name("remuda-hub-sqlite".into())
            .spawn(move || {
                let mut conn = match open_conn(&writer_path) {
                    Ok(conn) => {
                        let _ = ready_tx.send(Ok(()));
                        conn
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "hub sqlite open failed");
                        let _ = ready_tx.send(Err(err.to_string()));
                        return;
                    }
                };
                drop(ready_tx);
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
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(StoreError::Id(err)),
            Err(_) => return Err(StoreError::Closed),
        }
        Ok(Self {
            tx,
            readers: Arc::new(ReaderPool::new(path)),
            _join: Arc::new(StoreJoin {
                thread: Mutex::new(Some(thread)),
            }),
            test_fence_before_queue: Arc::new(std::sync::Mutex::new(None)),
            test_delete_device_before_queue: Arc::new(std::sync::Mutex::new(None)),
            test_fence_before_authority: Arc::new(std::sync::Mutex::new(None)),
            test_fence_before_task_mutate: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    /// Finish in-flight jobs, checkpoint WAL, and close every connection.
    pub async fn close(&self) {
        // Readers hold their own file handles, and a live reader keeps the WAL
        // from truncating. Retire them before asking the writer to checkpoint.
        self.readers.closed.store(true, Ordering::Release);
        if let Ok(mut idle) = self.readers.idle.lock() {
            idle.clear();
        }
        let (done, rx) = std::sync::mpsc::channel();
        if self.tx.send(Job::Stop(done)).is_err() {
            return;
        }
        let _ = tokio::task::spawn_blocking(move || rx.recv_timeout(BUSY_WAIT)).await;
    }

    /// Run one job on the writer thread.
    ///
    /// Every job carries a static `name` so the slow-job budget (see
    /// [`note_slow`]) can name the culprit in a `warn`. Time covers queue wait
    /// plus execution — queue wait is the stall this exists to expose — so it
    /// starts at the send, not when the writer picks the job up.
    pub(crate) async fn run_named<T, F>(&self, name: &'static str, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        // Queue wait is the symptom this budget exists to expose, so time from
        // the send, not from the moment the writer picks the job up.
        let queued = Instant::now();
        self.tx
            .send(Job::Run(Box::new(move |conn| {
                let _ = tx.send(f(conn));
            })))
            .map_err(|_| StoreError::Closed)?;
        let out = rx.await.map_err(|_| StoreError::Closed)?;
        note_slow(name, "write", queued.elapsed());
        out
    }

    /// Test-only: mark `instance_id` fenced and bump its lineage generation,
    /// the same authority effects the fence transaction F produces (minus F's
    /// cancellations, which land in ma-fence). Used by the initiator suite to
    /// put an instance in the post-fence state without the F machinery.
    #[doc(hidden)]
    pub async fn test_fence_instance(&self, instance_id: String) -> Result<(), StoreError> {
        self.run_named("test_fence_instance", move |conn| {
            test_apply_fence(conn, &instance_id)
        })
        .await
    }

    /// Test-only: arm the fence-between-authentication-and-commit seam.
    #[doc(hidden)]
    pub fn test_arm_fence_before_queue(&self, instance_id: String) {
        *self
            .test_fence_before_queue
            .lock()
            .expect("fence seam lock") = Some(instance_id);
    }

    /// Test-only: arm the device-deletion-between-authentication-and-commit
    /// seam.
    #[doc(hidden)]
    pub fn test_arm_delete_device_before_queue(&self, device_id: String) {
        *self
            .test_delete_device_before_queue
            .lock()
            .expect("device seam lock") = Some(device_id);
    }

    /// Test-only: delete one device row (the fence F also removes predecessor
    /// Agent devices).
    #[doc(hidden)]
    pub async fn test_delete_device(&self, device_id: String) -> Result<(), StoreError> {
        self.run_named("test_delete_device", move |conn| {
            conn.execute("DELETE FROM devices WHERE id = ?1", params![device_id])?;
            Ok(())
        })
        .await
    }

    /// Test-only: arm a fence that the NEXT `claim_gate_job` or
    /// `admit_node_op` writer job applies immediately before its authority
    /// check — a deterministic F between enqueue/admission and the next
    /// admission. Drained after one fire.
    #[doc(hidden)]
    pub fn test_arm_fence_before_authority_check(&self, instance_id: String) {
        *self
            .test_fence_before_authority
            .lock()
            .expect("authority seam lock") = Some(instance_id);
    }

    /// Test-only: drain (at most) the armed authority-check fence. Called
    /// inside the claim/admission writer jobs.
    pub(crate) fn take_test_authority_fence(&self) -> Option<String> {
        self.test_fence_before_authority
            .lock()
            .expect("authority seam lock")
            .take()
    }

    /// Test-only: arm a fence that the NEXT `mutate_task` writer job applies
    /// immediately before its authority check (task bind post-lease race).
    #[doc(hidden)]
    pub fn test_arm_fence_before_task_mutate(&self, instance_id: String) {
        *self
            .test_fence_before_task_mutate
            .lock()
            .expect("task mutate seam lock") = Some(instance_id);
    }

    /// Test-only: drain (at most) the armed task-mutate fence.
    pub(crate) fn take_test_task_mutate_fence(&self) -> Option<String> {
        self.test_fence_before_task_mutate
            .lock()
            .expect("task mutate seam lock")
            .take()
    }

    /// Run a read on the read-only pool instead of the writer thread.
    ///
    /// For reads that can be long: a journal window, an attachment blob, a list
    /// the web boot gate waits on. A read here never delays a write, and a
    /// write never delays it (hub-store-1). Use [`Store::run`] for writes and
    /// for short point reads that want the writer's own connection.
    pub(crate) async fn read<T, F>(&self, name: &'static str, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let queued = Instant::now();
        if self.readers.closed.load(Ordering::Acquire) {
            return Err(StoreError::Closed);
        }
        let permit = self
            .readers
            .permits
            .acquire()
            .await
            .map_err(|_| StoreError::Closed)?;
        let readers = Arc::clone(&self.readers);
        let out = tokio::task::spawn_blocking(move || {
            let conn = readers.take()?;
            let out = f(&conn);
            readers.put(conn);
            out
        })
        .await
        .map_err(|_| StoreError::Closed)?;
        drop(permit);
        note_slow(name, "read", queued.elapsed());
        out
    }

    /// Insert a device token hash.
    pub async fn insert_device(
        &self,
        name: String,
        token_hash: String,
        token_prefix: String,
    ) -> Result<Device, StoreError> {
        self.insert_device_as(name, token_hash, token_prefix, "human".into(), None)
            .await
    }

    /// Mint a credential with server-selected scope (never from request payload origin).
    pub async fn insert_device_as(
        &self,
        name: String,
        token_hash: String,
        token_prefix: String,
        kind: String,
        instance_id: Option<String>,
    ) -> Result<Device, StoreError> {
        self.run_named("insert_device_as", move |conn| {
            let id = new_id("dev").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO devices (id, name, token_hash, created_at, last_seen_at, token_prefix, kind, instance_id)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7)",
                params![id, name, token_hash, now, token_prefix, kind, instance_id],
            )?;
            Ok(Device { id, name, kind, instance_id })
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
        let Some(prefix) = crate::auth::token_prefix(&token).map(str::to_owned) else {
            return Ok(None);
        };
        let lookup_prefix = prefix.clone();
        let candidate = self
            .run_named("find_device_by_token", move |conn| {
                let read_row = |row: &rusqlite::Row<'_>| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                };
                let mut candidate = conn
                    .query_row(
                        "SELECT id, token_hash FROM devices WHERE token_prefix = ?1",
                        params![lookup_prefix],
                        read_row,
                    )
                    .optional()?;
                if candidate.is_none()
                    && let Some(id) = legacy_device_id
                {
                    candidate = conn.query_row(
                    "SELECT id, token_hash FROM devices WHERE id = ?1 AND token_prefix IS NULL",
                    params![id], read_row,
                ).optional()?;
                }
                Ok(candidate)
            })
            .await?;
        let Some((id, hash)) = candidate else {
            return Ok(None);
        };
        // Argon2 must not occupy the single SQLite writer: normal device
        // polling would otherwise delay unrelated native journal appends.
        let verified =
            tokio::task::spawn_blocking(move || verify(&token, &hash).then_some((id, hash)))
                .await
                .map_err(|_| StoreError::Id("device verification task failed".into()))?;
        let Some((id, hash)) = verified else {
            return Ok(None);
        };
        self.run_named("find_device_by_token", move |conn| {
            // Revocation or hash replacement while verification was in
            // flight must reject. Read scope now, not from the old snapshot.
            let device = conn
                .query_row(
                    "SELECT id, name, kind, instance_id FROM devices
                 WHERE id = ?1 AND token_hash = ?2
                   AND (token_prefix = ?3 OR token_prefix IS NULL)",
                    params![id, hash, prefix],
                    |row| {
                        Ok(Device {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            kind: row.get(2)?,
                            instance_id: row.get(3)?,
                        })
                    },
                )
                .optional()?;
            let Some(device) = device else {
                return Ok(None);
            };
            conn.execute(
                "UPDATE devices SET last_seen_at = ?1, token_prefix = ?2 WHERE id = ?3",
                params![now_rfc3339(), prefix, id],
            )?;
            Ok(Some(device))
        })
        .await
    }

    /// Enroll or refresh a host.
    ///
    /// D-018: the device access code is **not** accepted here. An existing host
    /// re-announces with its own stored node token; a new host presents a
    /// single-use enroll token minted by an authenticated device. Neither path
    /// lets a caller claim a `host_id` it cannot already authenticate as.
    pub async fn authenticate_host<F>(
        &self,
        request: HostAuthRequest,
        verify: F,
        hash_new: impl Fn(&str) -> Result<String, StoreError> + Send + 'static,
    ) -> Result<HostAuthOutcome, StoreError>
    where
        F: Fn(&str, &str) -> bool + Send + 'static,
    {
        self.run_named("authenticate_host", move |conn| {
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
            // Authenticated as *this* host; the claimed hello hostId is ignored (A1).
            if let Some((id, hash)) = candidate
                && verify(&request.presented, &hash) {
                    conn.execute("UPDATE hosts SET token_prefix = ?1 WHERE id = ?2", params![prefix, id])?;
                    let host = touch_host_online(conn, &id, &request.node_version)?;
                    return Ok(HostAuthOutcome::Authenticated {
                        host: Box::new(host),
                        node_token: None,
                    });
            }
            let now = now_rfc3339();
            let Some(enroll_id) = consume_enroll_token(conn, &request.presented, &now, &verify)?
            else {
                return Ok(HostAuthOutcome::Rejected);
            };
            let host_id = match request.hello_host_id {
                Some(id) => id,
                None => new_id("hst").map_err(|e| StoreError::Id(e.to_string()))?,
            };
            // An enroll token mints a *new* host only. Re-enrolling an existing
            // host_id requires that host's own token, so a leaked enroll token
            // cannot steal the routing slot of a live Node (A1/A2).
            if load_host(conn, &host_id)?.is_some() {
                tracing::warn!(
                    host_id = %host_id,
                    enroll_token_id = %enroll_id,
                    "enroll token presented for an existing host; rejecting"
                );
                return Ok(HostAuthOutcome::Rejected);
            }
            let node_token = crate::config::random_token();
            let token_hash = hash_new(&node_token)?;
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

    /// Store a hashed single-use node enroll token (D-018).
    ///
    /// `token_prefix` is the indexed lookup key (see [`crate::auth::token_prefix`]);
    /// `None` keeps the row unreachable by the fast path, which is what test
    /// fixtures with non-hex secrets want.
    pub async fn insert_enroll_token(
        &self,
        token_hash: String,
        token_prefix: Option<String>,
        created_by: String,
        expires_at: String,
    ) -> Result<String, StoreError> {
        self.run_named("insert_enroll_token", move |conn| {
            let prefix = token_prefix;
            let id = new_id("obj").map_err(|e| StoreError::Id(e.to_string()))?;
            conn.execute(
                "INSERT INTO enroll_tokens
                    (id, token_hash, token_prefix, created_by, expires_at, used, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
                params![
                    id,
                    token_hash,
                    prefix,
                    created_by,
                    expires_at,
                    now_rfc3339()
                ],
            )?;
            Ok(id)
        })
        .await
    }

    /// Stage one attachment, deduplicating by `(instance_id, digest)` (D-027).
    ///
    /// Re-uploading identical bytes for the same instance returns the existing
    /// row and refreshes its expiry, so a retried upload never doubles the
    /// instance's quota. Expired rows for that instance are swept first, which
    /// is the whole of the MVP's garbage collection — no cron.
    pub async fn insert_object(&self, new: NewObject) -> Result<ObjectRecord, StoreError> {
        self.run_named("insert_object", move |conn| {
            let now = now_rfc3339();
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM objects WHERE instance_id = ?1 AND expires_at <= ?2",
                params![new.instance_id, now],
            )?;
            let expires_at = rfc3339_after(new.ttl_seconds);
            if let Some(existing) = load_object_by_digest(&tx, &new.instance_id, &new.digest)? {
                tx.execute(
                    "UPDATE objects SET expires_at = ?1 WHERE id = ?2",
                    params![expires_at, existing.object_id],
                )?;
                tx.commit()?;
                return Ok(ObjectRecord {
                    anchor: existing.anchor,
                    expires_at,
                    ..existing
                });
            }
            let staged: i64 = tx.query_row(
                "SELECT COALESCE(SUM(byte_len), 0) FROM objects WHERE instance_id = ?1",
                params![new.instance_id],
                |row| row.get(0),
            )?;
            let byte_len = i64::try_from(new.bytes.len()).unwrap_or(i64::MAX);
            if staged.saturating_add(byte_len) > new.instance_budget {
                return Err(StoreError::Id(format!(
                    "RESOURCE_LIMIT: instance already stages {staged} bytes; the limit is {}",
                    new.instance_budget
                )));
            }
            let object_id = new_id("obj").map_err(|e| StoreError::Id(e.to_string()))?;
            // Derived, never caller-supplied: the blob name itself cannot
            // traverse. The original name is a separate column (D-027b).
            let stored_name = format!("{object_id}.{}", new.extension);
            let kind =
                remuda_protocol::hubnode::AttachmentKind::from_media_type(&new.media_type).as_str();
            tx.execute(
                "INSERT INTO objects
                    (id, instance_id, host_id, media_type, stored_name, original_name, kind,
                     digest, byte_len, bytes, created_by, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    object_id,
                    new.instance_id,
                    new.host_id,
                    new.media_type,
                    stored_name,
                    new.original_name,
                    kind,
                    new.digest,
                    byte_len,
                    new.bytes,
                    new.device_id,
                    now,
                    expires_at,
                ],
            )?;
            tx.commit()?;
            Ok(ObjectRecord {
                object_id,
                instance_id: new.instance_id,
                host_id: new.host_id,
                media_type: new.media_type,
                stored_name,
                original_name: new.original_name,
                kind: kind.to_owned(),
                digest: new.digest,
                byte_len,
                expires_at,
                anchor: None,
            })
        })
        .await
    }

    /// Attachment metadata without its bytes. On the reader pool: it always
    /// precedes a blob read, and the pair should not straddle two queues.
    pub async fn get_object(&self, id: String) -> Result<Option<ObjectRecord>, StoreError> {
        self.read("get_object", move |conn| load_object(conn, &id))
            .await
    }

    /// Record the 1-based `[Image #n]` anchor a send assigned to each object
    /// (2026-09-15). Drives the order `remuda_attachments_list` reports to the
    /// in-session agent. Runs in the same HTTP call that validated the send.
    pub async fn tag_object_anchors(&self, entries: Vec<(String, i64)>) -> Result<(), StoreError> {
        self.run_named("tag_object_anchors", move |conn| {
            for (object_id, anchor) in &entries {
                conn.execute(
                    "UPDATE objects SET anchor = ?1 WHERE id = ?2",
                    params![anchor, object_id],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Staged bytes, or `None` when the row is gone. On the reader pool: an
    /// attachment blob is the longest single read the Hub serves.
    pub async fn read_object_bytes(&self, id: String) -> Result<Option<Vec<u8>>, StoreError> {
        self.read("read_object_bytes", move |conn| {
            conn.query_row(
                "SELECT bytes FROM objects WHERE id = ?1",
                params![id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(StoreError::from)
        })
        .await
    }

    /// Resolve a Bearer token to a host id, using the same prefix index and
    /// verifier as the `/v1/node` handshake (D-027). `None` means the token is
    /// not a host token, leaving the device path free to try.
    pub async fn find_host_by_token<F>(
        &self,
        token: String,
        verify: F,
    ) -> Result<Option<String>, StoreError>
    where
        F: Fn(&str, &str) -> bool + Send + 'static,
    {
        self.run_named("find_host_by_token", move |conn| {
            let Some(prefix) = crate::auth::token_prefix(&token) else {
                return Ok(None);
            };
            let candidate = conn
                .query_row(
                    "SELECT id, token_hash FROM hosts WHERE token_prefix = ?1",
                    params![prefix],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            Ok(candidate
                .filter(|(_, hash)| verify(&token, hash))
                .map(|(id, _)| id))
        })
        .await
    }

    /// Mark every host offline. Hub restart has no live Node links until hello.
    pub async fn mark_all_hosts_offline(&self) -> Result<(), StoreError> {
        self.run_named("mark_all_hosts_offline", |conn| {
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
        self.run_named("mark_host_offline", move |conn| {
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
    ///
    /// Returns the number of instances swept plus the [`Settlement`] of the
    /// pending cards invalidated in the SAME transaction (c-cardsettle).
    pub async fn expire_lost_hosts(
        &self,
        grace_ms: u64,
    ) -> Result<(usize, Settlement), StoreError> {
        self.run_named("expire_lost_hosts", move |conn| {
            // BEGIN IMMEDIATE: this is a read-then-write job; a deferred tx
            // would take a SHARED lock on the SELECT and deadlock upgrading to
            // EXCLUSIVE against a pooled reader (see `immediate_tx`).
            let tx = immediate_tx(conn)?;
            let now = now_rfc3339();
            let grace = grace_ms.min(i64::MAX as u64) as i64;
            // Find the rows this sweep is about to end so their pending
            // interactions are invalidated in the SAME transaction
            // (c-cardsettle).
            //
            // ma-lineage r6 item 3(c): ONLY a chapter that actually reached a
            // LIVE lifecycle may become host-lost. `requested` rows belong to
            // expire_stale_requested (they keep their attested
            // create-never-acknowledged marker and fresh-recovery path), and a
            // terminal `failed` row (an attested launch failure) is left alone
            // — rewriting either to exited/host-lost would destroy the
            // evidence and block the seat and fresh recovery forever.
            let lost: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM instances
                     WHERE lifecycle IN ('starting','preparing','ready','running','closing','reconciling')
                       AND host_id IN (
                        SELECT id FROM hosts WHERE state != 'online' AND
                        (NOT EXISTS (SELECT 1 FROM ssh_hosts WHERE host_id = hosts.id) OR state = 'daemon-unreachable') AND
                        (julianday(?1) - julianday(COALESCE(offline_since, last_seen_at, created_at))) * 86400000 >= ?2
                     )",
                )?;
                let rows = stmt.query_map(params![&now, grace], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            // ma-lineage r5/r6 item 1+3: host loss is CONTACT loss, not process
            // end (D-019) — no ended_at. r6 uses a NEW marker constant
            // (HOST_LOST_MARKER) distinct from any legacy value, so rows
            // written before this change keep their old ended meaning; the
            // backfill stamps end evidence for those.
            let changed = tx.execute(
                "UPDATE instances SET lifecycle = 'exited', activity = 'idle',
                    connectivity = 'disconnected', last_error = ?3,
                    updated_at = ?1
                 WHERE lifecycle IN ('starting','preparing','ready','running','closing','reconciling')
                   AND host_id IN (
                    SELECT id FROM hosts WHERE state != 'online' AND
                    (NOT EXISTS (SELECT 1 FROM ssh_hosts WHERE host_id = hosts.id) OR state = 'daemon-unreachable') AND
                    (julianday(?1) - julianday(COALESCE(offline_since, last_seen_at, created_at))) * 86400000 >= ?2
                 )",
                params![&now, grace, HOST_LOST_MARKER],
            )?;
            let settlement = settle_instance_interactions(&tx, &lost, &now)?;
            tx.commit()?;
            Ok((changed, settlement))
        }).await
    }

    /// Record an operator action that outlives the entity it acted on.
    ///
    /// Deleting a session removes its journal, so the record of *who* deleted
    /// it has to live somewhere else. This table is that somewhere.
    pub async fn append_audit(
        &self,
        device_id: String,
        action: String,
        subject: Option<String>,
        detail: Value,
    ) -> Result<(), StoreError> {
        self.run_named("append_audit", move |conn| {
            conn.execute(
                "INSERT INTO audit_log (device_id, action, subject, detail_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    device_id,
                    action,
                    subject,
                    detail.to_string(),
                    now_rfc3339()
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Audit rows for one subject, oldest first (tests and support queries).
    pub async fn audit_for(&self, subject: String) -> Result<Vec<Value>, StoreError> {
        self.run_named("audit_for", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT device_id, action, subject, detail_json, created_at
                 FROM audit_log WHERE subject = ?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map(params![subject], |row| {
                    Ok(json!({
                        "deviceId": row.get::<_, String>(0)?,
                        "action": row.get::<_, String>(1)?,
                        "subject": row.get::<_, Option<String>>(2)?,
                        "detail": serde_json::from_str::<Value>(&row.get::<_, String>(3)?)
                            .unwrap_or(Value::Null),
                        "createdAt": row.get::<_, String>(4)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    // ----- Passkeys (D-030) -------------------------------------------------

    /// Persist a freshly registered credential. A repeated credential id is a
    /// conflict rather than a second row.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_passkey(
        &self,
        credential_id: String,
        public_key: String,
        counter: i64,
        transports: String,
        name: String,
        aaguid: Option<String>,
        created_by: String,
    ) -> Result<PasskeyRecord, StoreError> {
        self.run_named("insert_passkey", move |conn| {
            let id = new_id("cred").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO passkeys
                    (id, credential_id, public_key, counter, transports, name, aaguid,
                     created_by, created_at, last_used_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
                params![
                    id,
                    credential_id,
                    public_key,
                    counter,
                    transports,
                    name,
                    aaguid,
                    created_by,
                    now
                ],
            )
            .map_err(|err| {
                if matches!(
                    err.sqlite_error_code(),
                    Some(ErrorCode::ConstraintViolation)
                ) {
                    StoreError::DuplicateCredential
                } else {
                    StoreError::Sqlite(err)
                }
            })?;
            Ok(PasskeyRecord {
                id,
                credential_id,
                public_key,
                counter,
                transports,
                name,
                aaguid,
                created_by,
                created_at: now,
                last_used_at: None,
            })
        })
        .await
    }

    /// All registered credentials, newest first. A single-operator Hub keeps
    /// this list short; lookups by credential id are indexed.
    pub async fn list_passkeys(&self) -> Result<Vec<PasskeyRecord>, StoreError> {
        self.read("list_passkeys", |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, credential_id, public_key, counter, transports, name, aaguid,
                        created_by, created_at, last_used_at
                 FROM passkeys ORDER BY created_at DESC, id",
            )?;
            let rows = stmt
                .query_map([], PasskeyRecord::from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Look up one credential by its WebAuthn credential id.
    pub async fn passkey_by_credential_id(
        &self,
        credential_id: &str,
    ) -> Result<Option<PasskeyRecord>, StoreError> {
        let credential_id = credential_id.to_string();
        self.run_named("passkey_by_credential_id", move |conn| {
            conn.query_row(
                "SELECT id, credential_id, public_key, counter, transports, name, aaguid,
                        created_by, created_at, last_used_at
                 FROM passkeys WHERE credential_id = ?1",
                params![credential_id],
                PasskeyRecord::from_row,
            )
            .optional()
            .map_err(StoreError::from)
        })
        .await
    }

    /// Persist the post-ceremony credential JSON/counter and stamp last-used.
    pub async fn touch_passkey(
        &self,
        id: String,
        public_key: String,
        counter: i64,
    ) -> Result<(), StoreError> {
        self.run_named("touch_passkey", move |conn| {
            conn.execute(
                "UPDATE passkeys SET public_key = ?2, counter = ?3, last_used_at = ?4
                 WHERE id = ?1",
                params![id, public_key, counter, now_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    /// Change the human label. Missing row reports `None`.
    pub async fn rename_passkey(
        &self,
        id: String,
        name: String,
    ) -> Result<Option<PasskeyRecord>, StoreError> {
        self.run_named("rename_passkey", move |conn| {
            let updated = conn.execute(
                "UPDATE passkeys SET name = ?2 WHERE id = ?1",
                params![id, name],
            )?;
            if updated == 0 {
                return Ok(None);
            }
            let found = conn
                .query_row(
                    "SELECT id, credential_id, public_key, counter, transports, name, aaguid,
                            created_by, created_at, last_used_at
                     FROM passkeys WHERE id = ?1",
                    params![id],
                    PasskeyRecord::from_row,
                )
                .optional()?;
            Ok(found)
        })
        .await
    }

    /// Delete one credential. `false` when it was already gone, keeping
    /// `DELETE` idempotent.
    pub async fn delete_passkey(&self, id: String) -> Result<bool, StoreError> {
        self.run_named("delete_passkey", move |conn| {
            Ok(conn.execute("DELETE FROM passkeys WHERE id = ?1", params![id])? != 0)
        })
        .await
    }

    /// Base64url credential ids for register excludeCredentials.
    pub async fn passkey_credential_ids(&self) -> Result<Vec<String>, StoreError> {
        self.run_named("passkey_credential_ids", |conn| {
            let mut stmt = conn.prepare("SELECT credential_id FROM passkeys")?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Delete an instance the Hub never accepted (a fenced/never-forwarded
    /// `instance.create` row) and its Hub-side cascade, WITHOUT the
    /// terminal-lifecycle/current-chapter gates [`Self::delete_instance`]
    /// enforces. A never-forwarded create must leave no chapter behind:
    /// no Node ever saw it, so no tombstone is written either (F only
    /// tombstones chapters that actually existed on a Node).
    pub async fn purge_requested_instance(&self, instance_id: String) -> Result<(), StoreError> {
        self.run_named("purge_requested_instance", move |conn| {
            conn.execute(
                "DELETE FROM journal WHERE instance_id = ?1",
                params![&instance_id],
            )?;
            conn.execute(
                "DELETE FROM commands WHERE instance_id = ?1",
                params![&instance_id],
            )?;
            conn.execute(
                "DELETE FROM interactions WHERE instance_id = ?1",
                params![&instance_id],
            )?;
            conn.execute(
                "DELETE FROM fleet_members WHERE instance_id = ?1",
                params![&instance_id],
            )?;
            conn.execute(
                "UPDATE worktree_leases SET holder_instance_id = NULL, updated_at = ?2
                 WHERE holder_instance_id = ?1",
                params![&instance_id, now_rfc3339()],
            )?;
            conn.execute("DELETE FROM instances WHERE id = ?1", params![&instance_id])?;
            Ok(())
        })
        .await
    }

    /// Delete an instance the Hub never accepted
    ///
    /// Only a stopped Instance can be deleted; the caller is responsible for
    /// stopping it first (`force`). Returns `false` when the row is already
    /// gone, which is what makes `DELETE` idempotent, and an
    /// [`StoreError::Id`] naming the lifecycle when it is still live.
    ///
    /// Journal rows, queued commands, interactions, and fleet membership go
    /// with it: leaving any of them behind would resurrect the session in a
    /// list view or keep a command queued against an id that no longer exists.
    pub async fn delete_instance(&self, instance_id: String) -> Result<bool, StoreError> {
        self.run_named("delete_instance", move |conn| {
            let Some(instance) = load_instance(conn, &instance_id)? else {
                return Ok(false);
            };
            if !matches!(instance.lifecycle.as_str(), "exited" | "failed" | "closed") {
                return Err(StoreError::Id(format!(
                    "instance is {}; stop it before deleting",
                    instance.lifecycle
                )));
            }
            // ma-lineage r5 item 3: a NON-CURRENT CONTINUITY chapter is the
            // immutable lineage link its successors resolve parent ownership
            // through (parent_lineage_of reads the predecessor's row, and
            // fan-out/question routing count its descendants). Deleting it
            // silently orphans those successors (403s, broken routing, a freed
            // fan-out slot). Only the lineage's CURRENT chapter may be deleted;
            // older ended chapters must be retained.
            //
            // A plain (non-continuity) instance has NO lineages row; it is
            // always its own current chapter and is deletable.
            let lineage_exists: bool = conn
                .query_row(
                    "SELECT 1 FROM lineages WHERE lineage_id = ?1",
                    params![&instance.lineage_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            let is_current = !lineage_exists
                || conn
                    .query_row(
                        "SELECT 1 FROM lineages WHERE lineage_id = ?2
                            AND current_instance_id = ?1",
                        params![&instance_id, &instance.lineage_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
            if !is_current {
                return Err(StoreError::Conflict(format!(
                    "instance {instance_id} is a closed predecessor chapter; only the lineage's \
                     current chapter can be deleted (its successors resolve ownership through it)"
                )));
            }
            // ma-lineage r6 item 2: deleting the CURRENT chapter deletes the
            // WHOLE lineage (every chapter + the lineage row) in one
            // transaction. Otherwise the older chapters would be stranded — a
            // lineages row still naming a deleted current chapter, 409 on
            // deleting the predecessors forever, and 404 on resume. A plain
            // instance has no lineage row and is the sole member.
            let chapter_ids: Vec<String> = if lineage_exists {
                let mut stmt = conn.prepare(
                    "SELECT id FROM instances
                      WHERE lineage_id = ?1 ORDER BY generation ASC",
                )?;
                let rows =
                    stmt.query_map(params![&instance.lineage_id], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                vec![instance_id.clone()]
            };
            // Refuse if ANY chapter is still live: the HTTP handler stops the
            // current one before calling, but predecessors could still be live
            // in unusual states; require all terminal for a whole-lineage
            // delete.
            for chapter_id in &chapter_ids {
                let lifecycle: String = conn.query_row(
                    "SELECT lifecycle FROM instances WHERE id = ?1",
                    params![chapter_id],
                    |row| row.get(0),
                )?;
                if !matches!(lifecycle.as_str(), "exited" | "failed" | "closed") {
                    return Err(StoreError::Conflict(format!(
                        "chapter {chapter_id} is {lifecycle}; stop every chapter before deleting \
                         the lineage"
                    )));
                }
            }
            let tx = conn.transaction()?;
            for chapter_id in &chapter_ids {
                delete_instance_rows(&tx, chapter_id)?;
            }
            if lineage_exists {
                tx.execute(
                    "DELETE FROM lineages WHERE lineage_id = ?1",
                    params![&instance.lineage_id],
                )?;
            }
            tx.commit()?;
            Ok(true)
        })
        .await
    }

    // ── worktree leases (task-model t-pool) ───────────────────────────────

    /// Fetch the lease row for one directory key, if any.
    pub async fn get_worktree_lease(
        &self,
        host_id: String,
        workspace_id: String,
        dir_key: String,
    ) -> Result<Option<WorktreeLeaseRow>, StoreError> {
        self.run_named("get_worktree_lease", move |conn| {
            Ok(WorktreeLeaseRow::load(
                conn,
                &host_id,
                &workspace_id,
                &dir_key,
            )?)
        })
        .await
    }

    /// Classify the lease row's holder for a dispatch of `task_id` (t-bind,
    /// D-050 §2.3). Same-task sessions/tabs and a `pending:` claim carrying
    /// the same task are reclaimable; a missing instance and a `pending:`
    /// claim older than two minutes (a hub crashed mid-spawn) are stale and
    /// reclaimable on their exact value; a live holder of another task is
    /// busy. Returns None when the lease row does not exist.
    pub(crate) async fn classify_lease_holder(
        &self,
        host_id: String,
        workspace_id: String,
        dir_key: String,
        task_id: String,
    ) -> Result<Option<LeaseHolder>, StoreError> {
        self.run_named("classify_lease_holder", move |conn| {
            let Some(row) = WorktreeLeaseRow::load(conn, &host_id, &workspace_id, &dir_key)? else {
                return Ok(None);
            };
            let Some(holder) = row.holder_instance_id else {
                return Ok(Some(LeaseHolder::Free));
            };
            if let Some(claim_task) = holder
                .strip_prefix("pending:")
                .and_then(|rest| rest.split_once(':'))
                .map(|(task, _token)| task)
            {
                // Stale pre-spawn claim: reclaimable by anyone; SQL ages it
                // the same way.
                let stale: bool = conn.query_row(
                    "SELECT (julianday('now') - julianday(?1)) * 1440 > 2",
                    params![row.updated_at],
                    |stale| stale.get(0),
                )?;
                if claim_task == task_id || stale {
                    return Ok(Some(LeaseHolder::Reclaimable(holder)));
                }
                return Ok(Some(LeaseHolder::Busy(holder)));
            }
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM instances WHERE id = ?1)",
                params![holder],
                |value| value.get(0),
            )?;
            if !exists {
                return Ok(Some(LeaseHolder::Reclaimable(holder)));
            }
            // task_id is carried on the instance spec, not a column.
            let holder_task: Option<String> = conn.query_row(
                "SELECT json_extract(spec_json, '$.taskId') FROM instances WHERE id = ?1",
                params![holder],
                |value| value.get(0),
            )?;
            if holder_task.as_deref() == Some(task_id.as_str()) {
                Ok(Some(LeaseHolder::Reclaimable(holder)))
            } else {
                Ok(Some(LeaseHolder::Busy(holder)))
            }
        })
        .await
    }

    /// Atomically claim the attach lock on a leased directory before spawn
    /// (t-bind dispatch fold, D-050 §2.3).
    ///
    /// The holder is set with a single conditional UPDATE — it lands only
    /// where the row is currently `leased` and the holder is null, the exact
    /// `expect_holder` the caller classified as reclaimable (the same task's
    /// session/tabs, a stale instance holder), or a `pending:` claim older
    /// than two minutes (a hub that crashed mid-spawn). Two concurrent
    /// dispatches on one directory therefore serialize in the writer:
    /// exactly one claim lands and the other sees zero rows and must refuse
    /// dir-busy instead of launching. Returns true when claimed.
    pub async fn claim_worktree_lease(
        &self,
        host_id: String,
        workspace_id: String,
        dir_key: String,
        claim: String,
        expect_holder: Option<String>,
    ) -> Result<bool, StoreError> {
        self.run_named("claim_worktree_lease", move |conn| {
            let now = now_rfc3339();
            // A stale pre-spawn claim (>2 min) means the claiming hub died
            // before it could promote or release; reclaim it rather than
            // stalling the directory forever.
            let stale_pending =
                "(holder_instance_id LIKE 'pending:%' AND (julianday('now') - julianday(updated_at)) * 1440 > 2)";
            let sql = match &expect_holder {
                // The exact holder the caller classified, or a stale pending
                // claim.
                Some(_) => format!(
                    "UPDATE worktree_leases
                        SET holder_instance_id = ?1, updated_at = ?2
                      WHERE host_id = ?3 AND workspace_id = ?4 AND dir_key = ?5
                        AND state = 'leased'
                        AND (holder_instance_id = ?6 OR {stale_pending})"
                ),
                None => format!(
                    "UPDATE worktree_leases
                        SET holder_instance_id = ?1, updated_at = ?2
                      WHERE host_id = ?3 AND workspace_id = ?4 AND dir_key = ?5
                        AND state = 'leased'
                        AND (holder_instance_id IS NULL OR {stale_pending})"
                ),
            };
            let mut stmt = conn.prepare(&sql)?;
            let affected = match expect_holder {
                Some(expect) => stmt.execute(params![
                    claim, now, host_id, workspace_id, dir_key, expect
                ])?,
                None => stmt.execute(params![claim, now, host_id, workspace_id, dir_key])?,
            };
            Ok(affected > 0)
        })
        .await
    }

    /// Promote a pre-spawn claim token to the spawned instance's id. Only the
    /// claim that won the lock can promote, so a dispatch that lost the race
    /// never overwrites the winner's holder. Returns false when the row's
    /// holder no longer matches the claim.
    pub async fn promote_worktree_lease_holder(
        &self,
        host_id: String,
        workspace_id: String,
        dir_key: String,
        claim: String,
        instance_id: String,
    ) -> Result<bool, StoreError> {
        self.run_named("promote_worktree_lease_holder", move |conn| {
            let affected = conn.execute(
                "UPDATE worktree_leases
                    SET holder_instance_id = ?1, updated_at = ?2
                  WHERE host_id = ?3 AND workspace_id = ?4 AND dir_key = ?5
                    AND holder_instance_id = ?6",
                params![
                    instance_id,
                    now_rfc3339(),
                    host_id,
                    workspace_id,
                    dir_key,
                    claim
                ],
            )?;
            Ok(affected > 0)
        })
        .await
    }

    /// Clear the holder when it is exactly the claim given (abort path: the
    /// spawn failed and the directory must be free for the queued task).
    pub async fn release_worktree_lease_claim(
        &self,
        host_id: String,
        workspace_id: String,
        dir_key: String,
        claim: String,
    ) -> Result<(), StoreError> {
        self.run_named("release_worktree_lease_claim", move |conn| {
            conn.execute(
                "UPDATE worktree_leases
                    SET holder_instance_id = NULL, updated_at = ?1
                  WHERE host_id = ?2 AND workspace_id = ?3 AND dir_key = ?4
                    AND holder_instance_id = ?5",
                params![now_rfc3339(), host_id, workspace_id, dir_key, claim],
            )?;
            Ok(())
        })
        .await
    }

    /// Fetch the active lease a worker reclaim must consult.
    ///
    /// Matches the exact directory key *and* a pool slot derived from the
    /// worker name (`<name>-s<n>`): a dispatch can lease a pool named after
    /// the worker, in which case the slot's dir key is the suffixed form while
    /// the roster row carries the bare name. Only `leased`, refcount > 0 rows
    /// qualify (a parked slot has no holder and is safe to reclaim).
    pub async fn get_active_worktree_lease_for_name(
        &self,
        host_id: String,
        workspace_id: String,
        name: &str,
    ) -> Result<Option<WorktreeLeaseRow>, StoreError> {
        let name = name.to_string();
        self.run_named("get_active_worktree_lease_for_name", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM worktree_leases
                 WHERE host_id = ?1 AND workspace_id = ?2
                   AND state = 'leased' AND refcount > 0
                 ORDER BY refcount DESC, created_at",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![&host_id, &workspace_id], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            for id in ids {
                let row = WorktreeLeaseRow::load_by_id(conn, &id)?;
                if row.dir_key == name || is_pool_slot_of(&name, &row.dir_key) {
                    return Ok(Some(row));
                }
            }
            Ok(None)
        })
        .await
    }

    /// Record (or extend) a lease after a successful Node `worktree.lease`.
    ///
    /// Sharing is serial: a task not already on the row bumps refcount; a
    /// retry for a task already recorded is idempotent. The identity key is
    /// `(host_id, workspace_id, dir_key)`, so a reuse-to-root lease (name
    /// `None`, dir key `"."`) and a pool slot (`Some(slot)`) both land as
    /// ordinary rows.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_worktree_lease(
        &self,
        mode: String,
        host_id: String,
        workspace_id: String,
        dir_key: String,
        worktree_name: Option<String>,
        branch: Option<String>,
        base_oid: Option<String>,
        task_id: String,
        holder_instance_id: Option<String>,
    ) -> Result<WorktreeLeaseRow, StoreError> {
        self.run_named("record_worktree_lease", move |conn| {
            let now = now_rfc3339();
            if let Some(mut row) = WorktreeLeaseRow::load(conn, &host_id, &workspace_id, &dir_key)?
            {
                if !row.task_ids.contains(&task_id) {
                    row.task_ids.push(task_id.clone());
                }
                row.refcount = i64::try_from(row.task_ids.len()).unwrap_or(0);
                row.state = "leased".into();
                row.released_at = None;
                if row.branch.is_none() {
                    row.branch = branch.clone();
                }
                if row.base_oid.is_none() {
                    row.base_oid = base_oid.clone();
                }
                if row.worktree_name.is_none() {
                    row.worktree_name = worktree_name.clone();
                }
                if holder_instance_id.is_some() {
                    row.holder_instance_id = holder_instance_id.clone();
                }
                row.updated_at = now;
                row.save(conn)?;
                return Ok(row);
            }
            let row = WorktreeLeaseRow {
                id: new_id("wtl").map_err(|e| StoreError::Id(e.to_string()))?,
                mode,
                host_id,
                workspace_id,
                dir_key,
                worktree_name,
                branch,
                project_id: None,
                refcount: 1,
                state: "leased".into(),
                holder_instance_id,
                task_ids: vec![task_id],
                base_oid,
                created_at: now.clone(),
                updated_at: now,
                released_at: None,
            };
            row.insert(conn)?;
            Ok(row)
        })
        .await
    }

    /// Remove one task from a lease row after `worktree.return`.
    ///
    /// Refcount decrements; at zero a pool slot row rests as `parked`
    /// (warm slot, attach lock released), while a reuse row is deleted: the
    /// directory is the operator's own and the Hub keeps no claim on it.
    /// Returns `None` when no such row existed (or a reuse row was removed).
    pub async fn release_worktree_lease(
        &self,
        host_id: String,
        workspace_id: String,
        dir_key: String,
        task_id: String,
        node_state: String,
        branch: Option<String>,
    ) -> Result<Option<WorktreeLeaseRow>, StoreError> {
        self.run_named("release_worktree_lease", move |conn| {
            let Some(mut row) = WorktreeLeaseRow::load(conn, &host_id, &workspace_id, &dir_key)?
            else {
                return Ok(None);
            };
            row.task_ids.retain(|id| id != &task_id);
            row.refcount = i64::try_from(row.task_ids.len()).unwrap_or(0);
            row.updated_at = now_rfc3339();
            if row.refcount == 0 && row.mode == "reuse" {
                conn.execute("DELETE FROM worktree_leases WHERE id = ?1", params![row.id])?;
                return Ok(None);
            }
            if row.refcount == 0 {
                row.state = if node_state == "parked" {
                    "parked".to_string()
                } else {
                    "leased".to_string()
                };
                row.released_at = Some(now_rfc3339());
                row.holder_instance_id = None;
                row.branch = branch;
            }
            row.save(conn)?;
            Ok(Some(row))
        })
        .await
    }

    /// Leases currently carrying `holder_instance_id` (attach-lock holders).
    pub async fn worktree_leases_held_by_instance(
        &self,
        instance_id: String,
    ) -> Result<Vec<WorktreeLeaseRow>, StoreError> {
        self.run_named("worktree_leases_held_by_instance", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM worktree_leases WHERE holder_instance_id = ?1 ORDER BY created_at",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![&instance_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            ids.into_iter()
                .map(|id| WorktreeLeaseRow::load_by_id(conn, &id).map_err(StoreError::from))
                .collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    /// Active leases a task currently holds on a host.
    ///
    /// This is the production key for delete-time lease return: a session is
    /// deleted for a specific `task_id`, and `worktree.return` releases exactly
    /// that task's share (the refcount drops, a shared directory survives). It
    /// deliberately does not depend on `holder_instance_id`, which is only
    /// populated once a session is *attached* to the directory (the binding
    /// task wires attach; see evidence/task-model-2.md).
    pub async fn active_worktree_leases_for_task(
        &self,
        host_id: String,
        task_id: String,
    ) -> Result<Vec<WorktreeLeaseRow>, StoreError> {
        self.run_named("active_worktree_leases_for_task", move |conn| {
            // Match the task on the decoded array so `task_ids_json` stays an
            // internal detail rather than leaking a JSON1 expression to callers.
            let mut stmt = conn.prepare(
                "SELECT id, task_ids_json FROM worktree_leases
                 WHERE host_id = ?1 AND state = 'leased' AND refcount > 0
                 ORDER BY created_at",
            )?;
            let matched: Vec<String> = stmt
                .query_map(params![&host_id], |row| {
                    let id: String = row.get(0)?;
                    let raw: String = row.get(1)?;
                    Ok((id, raw))
                })?
                .filter_map(|row| row.ok())
                .filter_map(|(id, raw)| {
                    let ids: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
                    ids.contains(&task_id).then_some(id)
                })
                .collect();
            drop(stmt);
            matched
                .into_iter()
                .map(|id| WorktreeLeaseRow::load_by_id(conn, &id).map_err(StoreError::from))
                .collect()
        })
        .await
    }

    /// Append a Hub-authored terminal diagnostic to an instance journal.
    ///
    /// Only for instances the Node has abandoned (epoch changed, create never
    /// acknowledged, stop for an instance the Node does not know): the Hub
    /// takes the next seq, which is safe precisely because that Node will never
    /// emit another observation for the row. The event carries
    /// `payload.origin = "hub"` so a reader never mistakes it for a Node
    /// observation. Returns the appended record, or `None` when the instance is
    /// gone.
    pub async fn append_hub_diagnostic(
        &self,
        instance_id: String,
        native_name: String,
        message: String,
    ) -> Result<Option<JournalRecord>, StoreError> {
        self.run_named("append_hub_diagnostic", move |conn| {
            let Some(instance) = load_instance(conn, &instance_id)? else {
                return Ok(None);
            };
            let seq = instance.durable_seq.parse::<i64>().unwrap_or(0) + 1;
            if load_journal_row(conn, &instance_id, seq)?.is_some() {
                return Ok(None);
            }
            let event_id = new_id("evt").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            let event = json!({
                "eventId": event_id,
                "seq": seq.to_string(),
                "instanceId": instance_id,
                "kind": "lifecycle",
                "payload": {
                    "type": "native",
                    "topic": "diagnostic",
                    "origin": "hub",
                    "nativeName": native_name,
                    "severity": "warning",
                    "message": message,
                },
            });
            conn.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![instance_id, seq, event_id, event.to_string(), now],
            )?;
            conn.execute(
                "UPDATE instances SET durable_seq = ?1, updated_at = ?2 WHERE id = ?3",
                params![seq, now, instance_id],
            )?;
            Ok(Some(JournalRecord {
                instance_id,
                seq,
                event_id,
                event,
                observed_at: now,
            }))
        })
        .await
    }

    /// Append a Hub-authored journal event with an explicit payload.
    ///
    /// Used for the D-047 relay observations (`apiRoute` at launch, the
    /// per-stream counter record on `api.end`): counters and routing facts
    /// only, never bodies or headers. Same idempotent seq discipline as
    /// [`Self::append_hub_diagnostic`]; returns `None` when the instance is
    /// unknown or an event with this seq already exists.
    pub async fn append_hub_event(
        &self,
        instance_id: String,
        kind: &'static str,
        payload: Value,
    ) -> Result<Option<JournalRecord>, StoreError> {
        self.run_named("append_hub_event", move |conn| {
            let Some(instance) = load_instance(conn, &instance_id)? else {
                return Ok(None);
            };
            let seq = instance.durable_seq.parse::<i64>().unwrap_or(0) + 1;
            if load_journal_row(conn, &instance_id, seq)?.is_some() {
                return Ok(None);
            }
            let event_id = new_id("evt").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            let event = json!({
                "eventId": event_id,
                "seq": seq.to_string(),
                "instanceId": instance_id,
                "kind": kind,
                "payload": payload,
            });
            conn.execute(
                "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![instance_id, seq, event_id, event.to_string(), now],
            )?;
            conn.execute(
                "UPDATE instances SET durable_seq = ?1, updated_at = ?2 WHERE id = ?3",
                params![seq, now, instance_id],
            )?;
            Ok(Some(JournalRecord {
                instance_id,
                seq,
                event_id,
                event,
                observed_at: now,
            }))
        })
        .await
    }

    /// Record the epoch announced in `node.hello`, reporting a Node restart.
    ///
    /// Returns `true` only when a *different* non-empty epoch was already
    /// stored: a first announcement is not a restart, and a Node that omits
    /// `nodeEpoch` never claims one.
    pub async fn record_node_epoch(
        &self,
        host_id: String,
        epoch: Option<String>,
    ) -> Result<bool, StoreError> {
        self.run_named("record_node_epoch", move |conn| {
            let Some(epoch) = epoch.filter(|value| !value.is_empty()) else {
                return Ok(false);
            };
            let previous: Option<String> = conn
                .query_row(
                    "SELECT node_epoch FROM hosts WHERE id = ?1",
                    params![&host_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            conn.execute(
                "UPDATE hosts SET node_epoch = ?1 WHERE id = ?2",
                params![&epoch, &host_id],
            )?;
            Ok(previous.is_some_and(|prev| !prev.is_empty() && prev != epoch))
        })
        .await
    }

    /// Instances this Hub still counts as live that the Node no longer reports.
    ///
    /// Hub-owned projection only: the rows move to `exited` with `last_error`
    /// set, and no Node journal seq is forged (the Node stays the authority for
    /// its own cursor, as in [`Store::expire_lost_hosts`]).
    ///
    /// `requested` rows are deliberately out of scope. A create in flight is a
    /// Hub-side intent whose Node has not acknowledged it yet, so a Node that
    /// restarts in that window reports nothing for it without the row being
    /// lost — settling it here would kill a create that may yet land. Those
    /// rows have their own, age-bounded reaper:
    /// [`Store::expire_stale_requested`].
    pub async fn reconcile_reported_instances(
        &self,
        host_id: String,
        reported: Vec<String>,
        reason: String,
        epoch_changed: bool,
    ) -> Result<(Vec<String>, Settlement), StoreError> {
        self.run_named("reconcile_reported_instances", move |conn| {
            // The instance settlement and the interaction invalidation
            // (c-cardsettle) commit in ONE transaction: an inbox must never
            // observe an exited instance whose card is still actionable.
            let tx = immediate_tx(conn)?;
            // Normally only live chapters are reconciled against the Node's
            // inventory. ma-lineage r6 item 3(a): on a NODE EPOCH CHANGE the
            // old process is gone, so evidence-less host-lost/ambiguous
            // terminal rows the new Node does not report are ALSO reconciled
            // and stamped with process-end evidence. Rows that already carry
            // ended_at (genuine ends) and unacked `requested` rows stay out.
            let candidate_sql = if epoch_changed {
                "SELECT id FROM instances
                 WHERE host_id = ?1
                   AND (
                        lifecycle NOT IN ('exited', 'failed', 'requested', 'closed')
                        OR (lifecycle IN ('exited', 'failed') AND ended_at IS NULL)
                   )"
            } else {
                "SELECT id FROM instances
                 WHERE host_id = ?1 AND lifecycle NOT IN ('exited', 'failed', 'requested', 'closed')"
            };
            let mut stmt = tx.prepare(candidate_sql)?;
            let live: Vec<String> = stmt
                .query_map(params![&host_id], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            let lost: Vec<String> = live
                .into_iter()
                .filter(|id| !reported.iter().any(|seen| seen == id))
                .collect();
            let now = now_rfc3339();
            for id in &lost {
                if epoch_changed {
                    // The node epoch changed: the old process is provably
                    // gone — stamp ended_at so the row releases its seat/
                    // fan-out, preserving the reason as context.
                    tx.execute(
                        "UPDATE instances SET lifecycle = 'exited', activity = 'idle',
                            ended_at = COALESCE(ended_at, ?1),
                            last_error = CASE WHEN last_error IS NULL THEN ?2 ELSE last_error END,
                            updated_at = ?1
                         WHERE id = ?3",
                        params![&now, &reason, id],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE instances SET lifecycle = 'exited', activity = 'idle',
                            last_error = ?1, updated_at = ?2
                         WHERE id = ?3 AND ended_at IS NULL",
                        params![&reason, &now, id],
                    )?;
                }
            }
            // A generation that ended owns no still-answerable request.
            let settlement = settle_instance_interactions(&tx, &lost, &now)?;
            tx.commit()?;
            Ok((lost, settlement))
        })
        .await
    }

    /// Expire `requested` instances that never produced a Node receipt.
    ///
    /// A create the Node never acknowledged keeps occupying a placement slot
    /// forever otherwise. Returns `(host_id, instance_id)` for each expiry so
    /// the caller can publish a diagnostic.
    /// A create the Node never acknowledged keeps occupying a placement slot
    /// forever otherwise. Returns `(host_id, instance_id)` for each expiry so
    /// the caller can publish a diagnostic, plus the [`Settlement`] (c-cardsettle:
    /// such a row never journaled a card, so it is normally empty).
    pub async fn expire_stale_requested(
        &self,
        window_ms: u64,
    ) -> Result<(Vec<(String, String)>, Settlement), StoreError> {
        self.run_named("expire_stale_requested", move |conn| {
            let tx = immediate_tx(conn)?;
            let now = now_rfc3339();
            let window = window_ms.min(i64::MAX as u64) as i64;
            let mut stmt = tx.prepare(
                "SELECT id, host_id FROM instances
                 WHERE lifecycle = 'requested' AND durable_seq = 0 AND
                    (julianday(?1) - julianday(created_at)) * 86400000 >= ?2",
            )?;
            let stale: Vec<(String, String)> = stmt
                .query_map(params![&now, window], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            let stale_ids: Vec<String> = stale.iter().map(|(id, _)| id.clone()).collect();
            for id in &stale_ids {
                tx.execute(
                    "UPDATE instances SET lifecycle = 'failed', activity = 'idle',
                        last_error = ?3, updated_at = ?1,
                        ended_at = COALESCE(ended_at, ?1)
                     WHERE id = ?2 AND lifecycle = 'requested'",
                    params![&now, id, CREATE_NEVER_ACKNOWLEDGED_MARKER],
                )?;
            }
            // c-cardsettle: a create that never ran owns no answerable card;
            // the settle is defensive (the invariant is asserted by a test).
            let settlement = settle_instance_interactions(&tx, &stale_ids, &now)?;
            tx.commit()?;
            Ok((
                stale
                    .into_iter()
                    .map(|(id, host)| (host, id))
                    .collect::<Vec<_>>(),
                settlement,
            ))
        })
        .await
    }

    /// Settle a stop/close for an instance the Node does not know.
    ///
    /// Hub projection only: the row moves to `exited` so the slot is released
    /// and the caller never waits on a receipt that will not arrive. Returns
    /// whether THIS call made the row terminal plus the [`Settlement`] of the
    /// cards invalidated in that same change (c-cardsettle: explicit
    /// stop/kill/delete, or a stop for an instance the Node no longer knows).
    pub async fn settle_instance_exited(
        &self,
        instance_id: String,
        reason: String,
    ) -> Result<(bool, Settlement), StoreError> {
        self.run_named("settle_instance_exited", move |conn| {
            let tx = immediate_tx(conn)?;
            let now = now_rfc3339();
            let changed = tx.execute(
                "UPDATE instances SET lifecycle = 'exited', activity = 'idle',
                    last_error = ?1, updated_at = ?2, ended_at = COALESCE(ended_at, ?2)
                 WHERE id = ?3 AND lifecycle NOT IN ('exited', 'failed')",
                params![reason, &now, &instance_id],
            )?;
            let mut settlement = Settlement::default();
            if changed > 0 {
                settlement =
                    settle_instance_interactions(&tx, std::slice::from_ref(&instance_id), &now)?;
            }
            tx.commit()?;
            Ok((changed > 0, settlement))
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
        self.run_named("apply_inventory", move |conn| {
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
            if let Some(mut resources) = update.resources {
                stamp_resources_sampled_at(&mut resources, &now);
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
            if let Some(host_os) = update.host_os {
                conn.execute(
                    "UPDATE hosts SET os = ?1 WHERE id = ?2",
                    params![host_os, host_id],
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

    /// Persist an on-demand `host.resources` sample fetched during placement.
    ///
    /// Unlike [`Self::apply_inventory`] it touches nothing but `resources_json`
    /// (no lease/heartbeat side effects) and stamps the Hub-side sample time,
    /// so the freshness window is measured against the Hub clock rather than
    /// the Node's.
    pub async fn set_host_resources(
        &self,
        host_id: String,
        mut resources: Value,
    ) -> Result<(), StoreError> {
        self.run_named("set_host_resources", move |conn| {
            let now = now_rfc3339();
            stamp_resources_sampled_at(&mut resources, &now);
            conn.execute(
                "UPDATE hosts SET resources_json = ?1 WHERE id = ?2",
                params![resources.to_string(), host_id],
            )?;
            Ok(())
        })
        .await
    }

    /// All hosts.
    pub async fn list_hosts(&self) -> Result<Vec<HostRecord>, StoreError> {
        self.read("list_hosts", |conn| {
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
        self.run_named("get_host", move |conn| load_host(conn, &host_id))
            .await
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
        self.insert_instance_delegated(
            host_id,
            workspace_id,
            kind,
            driver,
            title,
            spec,
            InstanceDelegation::default(),
            // Non-delegated inserts are operator/internal paths.
            crate::agent_scope::CallerAuthority::internal(),
        )
        .await
    }

    /// Insert with explicit delegation-tree state; design §2.5.
    ///
    /// D-057 §7.3: an Agent child create carries the caller's authority pair,
    /// checked in this writer job before the instance row exists.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_instance_delegated(
        &self,
        host_id: String,
        workspace_id: Option<String>,
        kind: String,
        driver: String,
        title: Option<String>,
        spec: Value,
        delegation: InstanceDelegation,
        authority: crate::agent_scope::CallerAuthority,
    ) -> Result<InstanceRecord, StoreError> {
        self.run_named("insert_instance_delegated", move |conn| {
            let (initiator, device_id) = authority.as_check();
            check_initiator(conn, initiator, device_id)?;
            let host =
                load_host(conn, &host_id)?.ok_or_else(|| StoreError::Id("unknown host".into()))?;
            let running: i64 = conn.query_row(
                INSERT_SLOT_COUNT_SQL,
                params![host_id, REQUESTED_SLOT_WINDOW_MS as i64],
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
            let parent_id = spec
                .get("parentInstanceId")
                .and_then(Value::as_str)
                .map(str::to_string);
            if delegation.enforce_tree {
                validate_child_delegation(
                    conn,
                    parent_id.as_deref(),
                    &delegation.scope,
                    &delegation.grants,
                )?;
            }
            enforce_grant_uniqueness(conn, &delegation)?;
            let instance_id = new_id("ins").map_err(|e| StoreError::Id(e.to_string()))?;
            let journal_id = new_id("obj").map_err(|e| StoreError::Id(e.to_string()))?;
            let now = now_rfc3339();
            let connectivity = if host.online {
                "connected"
            } else {
                "disconnected"
            };
            let scope_json = serde_json::to_string(&delegation.scope)?;
            let grants_json = serde_json::to_string(&delegation.grants)?;
            let restart_json = match &delegation.restart {
                Some(policy) => Some(serde_json::to_string(policy)?),
                None => None,
            };
            conn.execute(
                "INSERT INTO instances
                    (id, host_id, workspace_id, kind, driver, lifecycle, activity, connectivity,
                     title, journal_id, durable_seq, spec_json, created_at, updated_at,
                     role, scope_json, grants_json, task_id,
                     lineage_id, generation, chapter_cause, fenced_at, restart_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'requested', 'unknown', ?6,
                         ?7, ?8, 0, ?9, ?10, ?10,
                         ?11, ?12, ?13, ?14,
                         ?1, 1, NULL, NULL, ?15)",
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
                    now,
                    delegation.role,
                    scope_json,
                    grants_json,
                    delegation.task_id,
                    restart_json,
                ],
            )?;
            // D-057 §5: a lineage row exists for every continuity instance —
            // one that holds any grant or carries a restart policy. Plain
            // instances need none.
            let continuity = !delegation.grants.is_empty() || delegation.restart.is_some();
            if continuity {
                let origin = spec.get("origin").cloned().unwrap_or(json!("agent"));
                let origin_spec_ref = serde_json::to_string(&json!({
                    "origin": origin,
                    "spec": spec,
                }))?;
                conn.execute(
                    "INSERT INTO lineages
                        (lineage_id, current_instance_id, generation, state,
                         paused_by_json, paused_at, restart_json, origin_spec_ref, updated_at)
                     VALUES (?1, ?1, 1, 'starting', NULL, NULL, ?2, ?3, ?4)",
                    params![instance_id, restart_json, origin_spec_ref, now],
                )?;
            }
            load_instance(conn, &instance_id)?
                .ok_or_else(|| StoreError::Id("instance insert missing".into()))
        })
        .await
    }

    /// Find an existing child that already resumes `session_id` on `driver`.
    ///
    /// Resume is retried by impatient clicks and by clients replaying a failed
    /// request, and each retry would otherwise launch another native process
    /// against the same conversation (D-026).
    pub async fn find_resume_child(
        &self,
        parent_instance_id: String,
        driver: String,
        session_id: String,
    ) -> Result<Option<InstanceRecord>, StoreError> {
        self.run_named("find_resume_child", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM instances
                 WHERE driver = ?1 AND lifecycle NOT IN ('exited', 'failed')
                 ORDER BY created_at DESC",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![driver], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            for id in ids {
                let Some(record) = load_instance(conn, &id)? else {
                    continue;
                };
                if record.resumed_from.as_deref() != Some(parent_instance_id.as_str()) {
                    continue;
                }
                let raw: String = conn.query_row(
                    "SELECT spec_json FROM instances WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )?;
                let spec: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
                if spec.get("resumeSessionId").and_then(Value::as_str) == Some(session_id.as_str())
                {
                    return Ok(Some(record));
                }
            }
            Ok(None)
        })
        .await
    }

    /// Ensure an instance row exists so Node can append journal before HTTP create.
    pub async fn ensure_instance(
        &self,
        host_id: String,
        instance_id: String,
    ) -> Result<InstanceRecord, StoreError> {
        self.run_named("ensure_instance", move |conn| {
            if is_deleted_instance(conn, &instance_id)? {
                return Err(StoreError::Id(format!(
                    "instance {instance_id} was deleted"
                )));
            }
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

    /// Mark a create that the Node rejected so it does not occupy a slot.
    ///
    /// Returns the [`Settlement`] of any pending cards invalidated in the same
    /// change (c-cardsettle: a failed launch leaves no answerable card).
    pub async fn fail_instance(
        &self,
        instance_id: String,
        last_error: String,
    ) -> Result<Settlement, StoreError> {
        self.run_named("fail_instance", move |conn| {
            let tx = immediate_tx(conn)?;
            let now = now_rfc3339();
            tx.execute(
                "UPDATE instances
                 SET lifecycle = 'failed', last_error = ?1, updated_at = ?2,
                     ended_at = COALESCE(ended_at, ?2)
                 WHERE id = ?3",
                params![last_error, &now, &instance_id],
            )?;
            let settlement =
                settle_instance_interactions(&tx, std::slice::from_ref(&instance_id), &now)?;
            tx.commit()?;
            Ok(settlement)
        })
        .await
    }

    /// List instances, optionally filtered by host. On the reader pool: the web
    /// boot gate waits on this list.
    pub async fn list_instances(
        &self,
        host_id: Option<String>,
    ) -> Result<Vec<InstanceRecord>, StoreError> {
        self.read("list_instances", move |conn| {
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
        self.run_named("get_instance", move |conn| {
            load_instance(conn, &instance_id)
        })
        .await
    }

    /// D-057 §5: lineage row for `lineage_id`, if the instance is a continuity
    /// lineage. Plain instances have no row.
    pub async fn get_lineage(
        &self,
        lineage_id: String,
    ) -> Result<Option<LineageRecord>, StoreError> {
        self.read("get_lineage", move |conn| load_lineage(conn, &lineage_id))
            .await
    }

    /// Chapters of a lineage in generation order, for `GET /v1/lineages/{id}`.
    pub async fn list_lineage_chapters(
        &self,
        lineage_id: String,
    ) -> Result<Vec<LineageChapter>, StoreError> {
        self.read("list_lineage_chapters", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, generation, chapter_cause, created_at, ended_at, fenced_at
                 FROM instances
                 WHERE COALESCE(lineage_id, id) = ?1
                 ORDER BY generation ASC, created_at ASC",
            )?;
            let rows = stmt
                .query_map(params![lineage_id], |row| {
                    let ended_at: Option<String> = row.get(4)?;
                    Ok(LineageChapter {
                        instance_id: row.get(0)?,
                        generation: row.get(1)?,
                        chapter_cause: row.get(2)?,
                        created_at: row.get(3)?,
                        // ma-lineage round 2: the immutable end-event
                        // timestamp, never the mutable updated_at.
                        ended_at,
                        fenced_at: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// ma-lineage r4 item 1: continuation/predecessor-close predicate that
    /// joins a loaded [`InstanceRecord`] with its durable `ended_at` column.
    /// A `failed` row counts as ended ONLY with recorded process-end evidence
    /// (a stamped ended_at from a classified end event / attested launch
    /// failure); an ambiguous failed row is treated as potentially live.
    pub async fn instance_has_process_end_evidence(
        &self,
        record: &InstanceRecord,
    ) -> Result<bool, StoreError> {
        let instance_id = record.instance_id.clone();
        let last_error = record.last_error.clone();
        let lifecycle = record.lifecycle.clone();
        let ended_at = self
            .read("instance_ended_at", move |conn| {
                conn.query_row(
                    "SELECT ended_at FROM instances WHERE id = ?1",
                    params![instance_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(StoreError::from)
            })
            .await?
            .flatten();
        Ok(lifecycle_has_process_end_evidence(
            &lifecycle,
            last_error.as_deref(),
            ended_at.as_deref(),
        ))
    }

    /// ma-lineage r7 item 1: the HTTP delete handler's side-effect-free
    /// pre-check / work plan. It answers in ONE read:
    ///
    /// * whether the addressed chapter may be deleted at all (a closed
    ///   predecessor chapter may not — [`DeletionScope::NonCurrent`]);
    /// * every chapter row a delete would remove, each with host and
    ///   lifecycle, so the handler can refuse while ANY chapter is still live
    ///   (before stop/lease/purge/audit) and purge EVERY chapter's Node data.
    pub async fn deletion_plan(&self, instance_id: &str) -> Result<DeletionScope, StoreError> {
        let instance_id = instance_id.to_owned();
        self.run_named("deletion_plan", move |conn| {
            let instance = load_instance(conn, &instance_id)?
                .ok_or_else(|| StoreError::Id("unknown instance".into()))?;
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM lineages WHERE lineage_id = ?1",
                    params![&instance.lineage_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Ok(DeletionScope::Plain(DeleteChapter {
                    instance_id: instance.instance_id.clone(),
                    host_id: instance.host_id.clone(),
                    lifecycle: instance.lifecycle.clone(),
                    task_id: instance.task_id.clone(),
                }));
            }
            let is_current = conn
                .query_row(
                    "SELECT 1 FROM lineages WHERE lineage_id = ?2
                        AND current_instance_id = ?1",
                    params![&instance_id, &instance.lineage_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !is_current {
                return Ok(DeletionScope::NonCurrent);
            }
            let mut stmt = conn.prepare(
                "SELECT id, host_id, lifecycle, task_id FROM instances
                 WHERE lineage_id = ?1 ORDER BY generation ASC, created_at ASC",
            )?;
            let chapters = stmt
                .query_map(params![&instance.lineage_id], |row| {
                    Ok(DeleteChapter {
                        instance_id: row.get(0)?,
                        host_id: row.get(1)?,
                        lifecycle: row.get(2)?,
                        task_id: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(DeletionScope::Current(chapters))
        })
        .await
    }

    /// D-057 §5: lineage edge used by `owns()`, the D-051 route and the
    /// `GET /v1/lineages/{id}` read predicate. See [`lineage_owns_conn`].
    pub async fn lineage_owns(
        &self,
        caller_instance_id: String,
        target_instance_id: String,
    ) -> Result<bool, StoreError> {
        self.read("lineage_owns", move |conn| {
            lineage_owns_conn(conn, &caller_instance_id, &target_instance_id)
        })
        .await
    }

    /// Stamped lineage id of an instance (its own id for a plain instance).
    pub async fn lineage_id_for_instance(
        &self,
        instance_id: String,
    ) -> Result<Option<String>, StoreError> {
        self.read("lineage_id_for_instance", move |conn| {
            lineage_id_of(conn, &instance_id)
        })
        .await
    }

    /// D-057 §5/§7.2 (ma-lineage): continuation resume in ONE writer
    /// transaction, with a compare-and-set on the lineage generation.
    ///
    /// Fences the lineage's current chapter, deletes every device row bound
    /// to it, bumps the generation, and inserts the successor chapter with the
    /// copied delegation plus its queued resume-create command. A
    /// continuation edge is not a delegation edge: the successor's parent is
    /// its predecessor's parent. `ma-fence` replaces this with the full
    /// `fence_lineage` transaction.
    pub async fn continuation_resume(
        &self,
        request: ContinuationResumeRequest,
    ) -> Result<ContinuationResumeResult, StoreError> {
        self.run_named("continuation_resume", move |conn| {
            continuation_resume_tx(conn, request)
        })
        .await
    }

    /// Read-only point load of one instance on the reader pool, for handlers
    /// that must keep observing committed state while a writer transaction is
    /// held open by a test hook.
    pub async fn get_instance_read(
        &self,
        instance_id: String,
    ) -> Result<Option<InstanceRecord>, StoreError> {
        self.read("get_instance_read", move |conn| {
            load_instance(conn, &instance_id)
        })
        .await
    }

    /// Merge model / effort into the instance spec so reload and list rows see them.
    pub async fn patch_instance_configure(
        &self,
        instance_id: String,
        payload: Value,
    ) -> Result<InstanceRecord, StoreError> {
        self.run_named("patch_instance_configure", move |conn| {
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
            // D-028 §9.1: whatever spelling arrives — the new object, the old
            // `{index, name}` object, a bare `effortName` string — is stored
            // as one normalized shape, so reload and list rows agree and no
            // consumer has to know the legacy tables.
            let incoming = payload.get("effort").cloned().or_else(|| {
                payload
                    .get("effortName")
                    .and_then(Value::as_str)
                    .map(|name| json!(name))
            });
            if let Some(incoming) = incoming {
                // Normalize with the instance's own kind, the same shared
                // per-harness map insert uses.
                let kind = object
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("claude");
                let effort_kind = remuda_protocol::effort_kind_from_str(kind);
                let selection = if let Some(name) = incoming.as_str() {
                    // A bare legacy word goes through the per-kind migrator.
                    remuda_protocol::normalize_legacy_effort(effort_kind, name)
                } else {
                    // The D-028 object: deserialize the current enum word.
                    serde_json::from_value::<remuda_protocol::EffortSelection>(incoming)?
                };
                object.insert(
                    "effort".into(),
                    json!({
                        "name": selection.level_name(),
                        "ultracode": selection.ultracode,
                    }),
                );
            }
            if let Some(mode) = payload.get("permissionMode").and_then(Value::as_str) {
                object.insert("permissionMode".into(), json!(mode));
            }
            let now = now_rfc3339();
            // The spec merge and the authoritative merge counter commit in one
            // UPDATE (D-055 round 2, item 4).
            conn.execute(
                "UPDATE instances
                 SET spec_json = ?1, updated_at = ?2, configure_seq = configure_seq + 1
                 WHERE id = ?3",
                params![Value::Object(object).to_string(), now, instance_id],
            )?;
            load_instance(conn, &instance_id)?
                .ok_or_else(|| StoreError::Id("unknown instance".into()))
        })
        .await
    }

    /// Queue a command. Same `command_id` or idempotency key returns the original row.
    ///
    /// D-057 §7.3: when `initiator` is set the commit-time authority check
    /// runs inside this writer job, so a request authenticated before a fence
    /// cannot be admitted after it.
    #[allow(clippy::too_many_arguments)]
    pub async fn queue_command(
        &self,
        command_id: Option<String>,
        instance_id: Option<String>,
        host_id: String,
        operation: String,
        payload: Value,
        idempotency_key: Option<String>,
        initiator: Option<remuda_protocol::Initiator>,
        initiator_device_id: Option<String>,
    ) -> Result<(CommandRecord, bool), StoreError> {
        // Drain the test-only fence seam at enqueue time; it is applied at the
        // top of the writer job below.
        let armed_fence = self
            .test_fence_before_queue
            .lock()
            .expect("fence seam lock")
            .take();
        let armed_device_delete = self
            .test_delete_device_before_queue
            .lock()
            .expect("device seam lock")
            .take();
        self.run_named("queue_command", move |conn| {
            // Test-only seam: a fence armed by the test lands inside THIS
            // writer job, immediately before the authority check — a
            // deterministic F-between-authentication-and-commit.
            if let Some(fenced_instance) = armed_fence {
                test_apply_fence(conn, &fenced_instance)?;
            }
            if let Some(device_id) = armed_device_delete {
                conn.execute("DELETE FROM devices WHERE id = ?1", params![device_id])?;
            }
            // D-057 §7.3: re-check authority inside the writer, before any
            // row exists. On Fenced the job errors and writes nothing.
            check_initiator(conn, initiator.as_ref(), initiator_device_id.as_deref())?;
            if let Some(key) = idempotency_key.as_ref()
                && let Some(existing) = load_command_by_key(conn, key)?
            {
                if existing.payload != payload || existing.operation != operation {
                    return Err(StoreError::Id(
                        "idempotency key reused with a different payload".into(),
                    ));
                }
                // Full (commandId, key) identity: a key that resolves to a row
                // cannot be presented under a different commandId.
                if let Some(cmd) = command_id.as_ref()
                    && cmd != &existing.command_id
                {
                    return Err(StoreError::Id(
                        "idempotency key reused with a different commandId".into(),
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
                        // Key identity checked HERE, inside the writer job:
                        // two concurrent same-id first POSTs that both missed
                        // the HTTP-level lookup serialize at this closure, so
                        // the divergent-key loser cannot take the
                        // `!created` replay branch. An omitted replay key is
                        // allowed; any present key must equal the stored one.
                        match (idempotency_key.as_ref(), existing.idempotency_key.as_ref()) {
                            (None, _) => {}
                            (Some(incoming), Some(stored)) if incoming == stored => {}
                            _ => {
                                return Err(StoreError::Id(
                                    "commandId reused with a different idempotency key".into(),
                                ));
                            }
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
                     payload_json, idempotency_key, created_at, updated_at,
                     initiator_instance_id, initiator_lineage_id, initiator_generation,
                     initiator_device_id)
                 VALUES (?1, ?2, ?3, ?4, 'queued', 'clear', 0, ?5, ?6, ?7, ?7, ?8, ?9, ?10, ?11)",
                params![
                    command_id,
                    instance_id,
                    host_id,
                    operation,
                    payload.to_string(),
                    idempotency_key,
                    now,
                    initiator.as_ref().map(|i| i.instance_id.clone()),
                    initiator.as_ref().map(|i| i.lineage_id.clone()),
                    initiator.as_ref().map(|i| i.generation),
                    initiator_device_id,
                ],
            )?;
            let row = load_command(conn, &command_id)?
                .ok_or_else(|| StoreError::Id("command insert missing".into()))?;
            Ok((row, true))
        })
        .await
    }

    /// Persist forward intent. Returns false if already forwarded (do not resend).
    ///
    /// D-057 §7.3: re-checks the initiator and device id STAMPED ON THE ROW,
    /// inside the writer job. A same-id replay of a held row by a since-fenced
    /// initiator is refused here, before any frame is written to a Node.
    pub async fn mark_forward_intent(&self, command_id: String) -> Result<bool, StoreError> {
        self.run_named("mark_forward_intent", move |conn| {
            let Some(row) = load_command(conn, &command_id)? else {
                return Err(StoreError::Id("unknown command".into()));
            };
            check_initiator(
                conn,
                row.initiator.as_ref(),
                row.initiator_device_id.as_deref(),
            )?;
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

    /// The RPC accept deadline elapsed: the request is on the wire and the
    /// Node is still executing it (protocol §2.5: a timeout returns `unknown`,
    /// the command is never resent). Record that convergence is now delegated
    /// to the mirrored journal — `unknown → reconciling` — from which the
    /// journaled accept wins and flips resolution back to `clear`.
    pub async fn mark_reconciling(&self, command_id: String) -> Result<CommandRecord, StoreError> {
        self.run_named("mark_reconciling", move |conn| {
            let now = now_rfc3339();
            conn.execute(
                "UPDATE commands SET resolution = 'reconciling', updated_at = ?1
                 WHERE id = ?2 AND state = 'queued' AND resolution = 'unknown'",
                params![now, command_id],
            )?;
            load_command(conn, &command_id)?.ok_or_else(|| StoreError::Id("unknown command".into()))
        })
        .await
    }

    /// Record the driver the Node reports it actually built for an instance.
    ///
    /// The Hub's stored driver is a *request* until the Node answers. When the
    /// two differ the record must follow the Node, because every later screen
    /// read, nudge and watch classification assumes the row names the carrier
    /// that is really running — a roster that said `claude-pty` over a live
    /// `claude-print` made all of them inexplicable
    /// (docs/design/evidence/dispatch-driver-1.md).
    ///
    /// Returns the driver now on the row, or `None` if the instance is unknown.
    pub async fn reconcile_instance_driver(
        &self,
        instance_id: String,
        driver: String,
    ) -> Result<Option<String>, StoreError> {
        self.run_named("reconcile_instance_driver", move |conn| {
            let now = now_rfc3339();
            conn.execute(
                "UPDATE instances SET driver = ?1, updated_at = ?2 WHERE id = ?3 AND driver != ?1",
                params![driver, now, instance_id],
            )?;
            conn.query_row(
                "SELECT driver FROM instances WHERE id = ?1",
                params![instance_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
        })
        .await
    }

    /// Read an instance's raw create spec JSON (D-047 route echo validation).
    pub async fn get_instance_spec_json(
        &self,
        instance_id: String,
    ) -> Result<Option<Value>, StoreError> {
        self.run_named("get_instance_spec_json", move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT spec_json FROM instances WHERE id = ?1",
                    params![instance_id],
                    |row| row.get(0),
                )
                .optional()?;
            raw.map(|raw| serde_json::from_str(&raw))
                .transpose()
                .map_err(StoreError::from)
        })
        .await
    }

    /// Project the Node-observed API route onto an instance (D-047).
    ///
    /// Callers must validate the echo first — this writes verbatim and is the
    /// one place the observed route enters the projection. Returns the stored
    /// route JSON, or `None` when the instance does not exist.
    pub async fn reconcile_instance_api_route(
        &self,
        instance_id: String,
        route: &remuda_protocol::ApiRoute,
    ) -> Result<Option<()>, StoreError> {
        let encoded = serde_json::to_string(route)?;
        self.run_named("reconcile_instance_api_route", move |conn| {
            let now = now_rfc3339();
            let affected = conn.execute(
                "UPDATE instances SET api_route_json = ?1, updated_at = ?2 WHERE id = ?3",
                params![encoded, now, instance_id],
            )?;
            Ok((affected > 0).then_some(()))
        })
        .await
    }

    /// Active (non-terminal) instances whose observed route proxies through
    /// `via_host_id`. Used when the proxy host's link drops: only instances
    /// actually egressing on that host go `blocked{api-route-down}`.
    pub async fn instances_routed_via(
        &self,
        via_host_id: String,
    ) -> Result<Vec<String>, StoreError> {
        self.run_named("instances_routed_via", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM instances
                 WHERE json_extract(api_route_json, '$.mode') = 'via'
                   AND json_extract(api_route_json, '$.viaHostId') = ?1
                   AND (lifecycle IN
                    ('preparing', 'starting', 'ready', 'running', 'closing', 'reconciling')
                    OR lifecycle = 'requested')",
            )?;
            let ids = stmt
                .query_map(params![via_host_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ids)
        })
        .await
    }

    /// Node RPC success → `accepted`, only from `queued`.
    ///
    /// There is no fourth state to recover: a forwarded send that the Node
    /// never acknowledges stays `queued` with resolution `unknown` /
    /// `reconciling` until the Node's mirrored journal says otherwise (§2.5,
    /// §12.2). A `settled` row — including a `rejected` settlement — is
    /// terminal and is never regressed by a later accept.
    pub async fn mark_accepted(&self, command_id: String) -> Result<CommandRecord, StoreError> {
        self.run_named("mark_accepted", move |conn| {
            let now = now_rfc3339();
            conn.execute(
                "UPDATE commands
                 SET state = 'accepted', resolution = 'clear',
                     settlement_outcome = NULL, settlement_reason = NULL, updated_at = ?1
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
        self.run_named("mark_settlement_timed_out", move |conn| {
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

    /// Node-reported completion → `settled` with outcome `completed`.
    ///
    /// This is the path the Node's own settle frame takes (`ws.rs`). It always
    /// records the `completed` settlement and clears any rejection reason, so a
    /// settled row can never carry a stale failure reason. The first terminal
    /// settlement is stable: an already-`settled` row is returned unchanged
    /// rather than regressed (protocol §2.5, terminal states do not revert).
    pub async fn mark_settled(
        &self,
        command_id: String,
        host_id: String,
    ) -> Result<CommandRecord, StoreError> {
        self.run_named("mark_settled", move |conn| {
            let Some(row) = load_command(conn, &command_id)? else {
                return Err(StoreError::Id("unknown command".into()));
            };
            if row.host_id != host_id {
                return Err(StoreError::Id("command belongs to another host".into()));
            }
            if row.state != "settled" {
                let now = now_rfc3339();
                conn.execute(
                    "UPDATE commands
                     SET state = 'settled', resolution = 'clear',
                         settlement_outcome = 'completed', settlement_reason = NULL,
                         updated_at = ?1
                     WHERE id = ?2",
                    params![now, command_id],
                )?;
            }
            load_command(conn, &command_id)?.ok_or_else(|| StoreError::Id("unknown command".into()))
        })
        .await
    }

    /// Load a command.
    pub async fn get_command(
        &self,
        command_id: String,
    ) -> Result<Option<CommandRecord>, StoreError> {
        self.run_named("get_command", move |conn| load_command(conn, &command_id))
            .await
    }

    /// Recent commands for one instance, newest first, bounded by `limit`.
    ///
    /// Backs `GET /v1/instances/{id}/commands`: each row carries operation,
    /// the three-state state, resolution, the §2.5 settlement projection and
    /// timestamps so an operator can see how a send resolved.
    pub async fn list_instance_commands(
        &self,
        instance_id: String,
        limit: usize,
    ) -> Result<Vec<CommandRecord>, StoreError> {
        let limit = limit.clamp(1, 200) as i64;
        self.read("list_instance_commands", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, instance_id, host_id, operation, state, resolution, forwarded,
                        payload_json, idempotency_key, created_at, updated_at,
                        settlement_outcome, settlement_reason,
                        settlement_http_status, settlement_http_body,
                        initiator_instance_id, initiator_lineage_id, initiator_generation,
                        initiator_device_id
                 FROM commands
                 WHERE instance_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![instance_id, limit], command_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Settle a still-`queued` command with a `rejected` settlement.
    ///
    /// This is the *pre-dispatch / explicit-rejection* case the §2.5 diagram
    /// routes to `settled` with settlement outcome `rejected`: the Node
    /// durably answered the RPC with an error, so the Hub has positive
    /// evidence the command did not run. It is **not** used for a lost reply or
    /// a Node that never answers — without evidence the command did not run,
    /// §2.5 forbids marking it rejected, and such a row rests at `queued` with
    /// resolution `unknown` / `reconciling` for the mirrored journal to
    /// converge.
    ///
    /// Returns `None` when the row already advanced (an `accepted` that raced
    /// the error, or an already-`settled` terminal); terminal states never
    /// revert (docs/design/evidence/instance-send-1.md).
    pub async fn reject_command(
        &self,
        command_id: String,
        reason: String,
    ) -> Result<Option<CommandRecord>, StoreError> {
        self.reject_command_outcome(command_id, reason, None, None)
            .await
    }

    /// [`Self::reject_command`] variant that also persists the exact HTTP
    /// status and JSON body the first attempt answered with (D-055 round 2).
    /// A replay of the command returns that pair verbatim, so a pre-dispatch
    /// or post-forward refusal can never become a replayed success.
    pub async fn reject_command_outcome(
        &self,
        command_id: String,
        reason: String,
        http_status: Option<i64>,
        http_body: Option<String>,
    ) -> Result<Option<CommandRecord>, StoreError> {
        self.run_named("reject_command_outcome", move |conn| {
            let now = now_rfc3339();
            let changed = conn.execute(
                "UPDATE commands
                 SET state = 'settled', resolution = 'clear',
                     settlement_outcome = 'rejected', settlement_reason = ?1,
                     settlement_http_status = ?2, settlement_http_body = ?3,
                     updated_at = ?4
                 WHERE id = ?5 AND state = 'queued'",
                params![reason, http_status, http_body, now, command_id],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            load_command(conn, &command_id)
        })
        .await
    }

    /// Append a mirrored event. `seq` None assigns durableSeq+1.
    /// Append a mirrored event. `seq` None assigns durableSeq+1.
    ///
    /// c-cardsettle: this is the single-event case of
    /// [`Store::append_journal_batch`] and delegates to it, so the instance
    /// terminal UPDATE and the interaction settlement inside
    /// `append_loaded_event` commit as ONE transaction — the old auto-commit
    /// path could durably write one while failing the other, leaving an exited
    /// instance with a still-actionable card.
    pub async fn append_journal(
        &self,
        host_id: String,
        instance_id: String,
        seq: Option<i64>,
        event: Value,
    ) -> Result<JournalAppend, StoreError> {
        let mut out = self
            .append_journal_batch(host_id, instance_id, seq, vec![event])
            .await?;
        Ok(out.pop().expect("one input event yields one append result"))
    }

    /// Append one chunk of a frame's events in a single writer job /
    /// transaction.
    ///
    /// The ws layer cuts a multi-event frame into chunks of
    /// [`APPEND_CHUNK_MAX`] and awaits each call, so a 256-event replay costs a
    /// handful of short jobs the writer can yield between rather than 256 jobs
    /// (or one job holding the connection for the whole frame) (hub-store-1).
    ///
    /// Per-event sequence checks, replay handling and the watermark are
    /// identical to [`Store::append_journal`]; the only difference is
    /// atomicity — a gap on any event rolls the whole chunk back instead of
    /// leaving a partial prefix durable. Returns one [`JournalAppend`] per
    /// input event, in seq order.
    pub async fn append_journal_batch(
        &self,
        host_id: String,
        instance_id: String,
        first_seq: Option<i64>,
        events: Vec<Value>,
    ) -> Result<Vec<JournalAppend>, StoreError> {
        self.run_named("append_journal_batch", move |conn| {
            // BEGIN IMMEDIATE: the append reads the instance row and journal
            // cursor and then writes; a deferred tx would take SHARED first and
            // deadlock upgrading to EXCLUSIVE against a pooled reader.
            let tx = immediate_tx(conn)?;
            let inst = load_instance(&tx, &instance_id)?
                .ok_or_else(|| StoreError::Id("unknown instance".into()))?;
            if inst.host_id != host_id {
                return Err(StoreError::Id("instance belongs to another host".into()));
            }
            let mut cursor =
                AppendCursor::new(inst.durable_seq.parse::<i64>().unwrap_or(0), first_seq);
            let mut out = Vec::with_capacity(events.len());
            for event in events {
                out.push(append_loaded_event(
                    &tx,
                    &host_id,
                    &instance_id,
                    &mut cursor,
                    event,
                )?);
            }
            tx.commit()?;
            Ok(out)
        })
        .await
    }

    /// Inclusive durable-seq watermarks for every instance on `host_id`.
    pub async fn list_instance_watermarks(
        &self,
        host_id: String,
    ) -> Result<Vec<InstanceWatermark>, StoreError> {
        self.run_named("list_instance_watermarks", move |conn| {
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

    /// Pending interactions, optionally filtered. On the reader pool: the web
    /// boot gate waits on this list, so it must not queue behind ingest.
    pub async fn list_interactions(
        &self,
        host_id: Option<String>,
        instance_id: Option<String>,
        kind: Option<String>,
        pending_only: bool,
    ) -> Result<Vec<InteractionRecord>, StoreError> {
        self.read("list_interactions", move |conn| {
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

    /// How long a non-pending interaction stays visible in the operator
    /// inbox's departed/ended presentation after it settled (c-cardsettle).
    /// Pending rows are returned regardless of age; this bounds only the
    /// terminal history a poll carries (rows are also deleted with their
    /// instance).
    pub const DEPARTED_INTERACTION_RETENTION_SECS: i64 = 24 * 60 * 60;

    /// Inbox feed: every actionable `pending` interaction PLUS recently
    /// settled ended/expired rows so the UI can render its 已离队/departed
    /// presentation after a reload or poll (c-cardsettle). The pure pending
    /// badge counters keep using [`Self::list_interactions`] with
    /// `pending_only=true`; terminal rows never count as actionable.
    pub async fn list_inbox_interactions(
        &self,
        host_id: Option<String>,
        instance_id: Option<String>,
        kind: Option<String>,
    ) -> Result<Vec<InteractionRecord>, StoreError> {
        self.read("list_inbox_interactions", move |conn| {
            let mut sql = String::from(
                "SELECT id, instance_id, host_id, kind, state, blocking, payload_json, created_at, updated_at
                 FROM interactions
                 WHERE (state = 'pending'
                        OR (state IN ('expired', 'invalidated')
                            AND (julianday('now') - julianday(updated_at)) * 86400 <= ?1))",
            );
            let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(
                Self::DEPARTED_INTERACTION_RETENTION_SECS,
            )];
            if let Some(host_id) = &host_id {
                sql.push_str(" AND host_id = ?");
                args.push(Box::new(host_id.clone()));
            }
            if let Some(instance_id) = &instance_id {
                sql.push_str(" AND instance_id = ?");
                args.push(Box::new(instance_id.clone()));
            }
            if let Some(kind) = &kind {
                sql.push_str(" AND kind = ?");
                args.push(Box::new(kind.clone()));
            }
            sql.push_str(" ORDER BY created_at");
            let mut stmt = conn.prepare(&sql)?;
            let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                args.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), interaction_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
        .await
    }

    /// c-cardsettle r2 item 3 / r3 item 2: of the given ids, return those whose
    /// DURABLE Hub row is terminal, regardless of the 24 h inbox display
    /// retention, PLUS tombstone ids left behind when a terminal instance was
    /// deleted. Display retention decides whether a departed row is SHOWN; this
    /// decides authoritative dedup, so a restarted Node's `interaction.list`
    /// can never re-queue the same id as pending (even after the Hub row was
    /// deleted with a rejected purge) and put it back on the badge.
    pub async fn terminal_interaction_ids(
        &self,
        ids: Vec<String>,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        if ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        self.run_named("terminal_interaction_ids", move |conn| {
            let placeholders = vec!["?"; ids.len()].join(",");
            // Tombstones only ever record terminal/non-answerable state (the
            // delete transaction snapshots the row as it stood), so every match
            // there is authoritative; live rows must additionally be terminal.
            let sql = format!(
                "SELECT id FROM interaction_tombstones WHERE id IN ({placeholders})
                 UNION
                 SELECT id FROM interactions
                 WHERE id IN ({placeholders})
                   AND state IN ('expired', 'invalidated', 'answer-committed', 'resolved')"
            );
            let params: Vec<&dyn rusqlite::types::ToSql> = ids
                .iter()
                .chain(ids.iter())
                .map(|id| id as &dyn rusqlite::types::ToSql)
                .collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params.as_slice(), |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()
                .map_err(StoreError::from)
        })
        .await
    }

    /// Test-only seam: age one interaction's `updated_at` past the departed
    /// retention (c-cardsettle r2 item 3).
    pub async fn test_backdate_interaction(
        &self,
        interaction_id: String,
        stamp: String,
    ) -> Result<(), StoreError> {
        self.run_named("test_backdate_interaction", move |conn| {
            conn.execute(
                "UPDATE interactions SET updated_at = ?1 WHERE id = ?2",
                params![stamp, interaction_id],
            )?;
            Ok(())
        })
        .await
    }

    /// c-cardsettle r3 item 4 / r4 item 7: recent `(instance_id,
    /// interaction_id, reason)` triples invalidated within the replay window.
    /// The reason is read from the durable payload (entity/interaction
    /// `resolution.value.reason`), so a reconnect replay never mislabels a
    /// non-generation-ended invalidation (e.g. a transcript picker demotion).
    /// Tombstones only retain state, so they report generation-ended.
    pub async fn recent_invalidated_interactions(
        &self,
    ) -> Result<Vec<(String, String, String)>, StoreError> {
        let rows = self
            .invalidated_interactions_page(Some(5), None, SETTLEMENT_LAG_PAGE)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(instance_id, interaction_id, reason, _updated_at)| {
                (instance_id, interaction_id, reason)
            })
            .collect())
    }

    /// c-cardsettle r6 item 2: one BOUNDED page (at most
    /// [`SETTLEMENT_LAG_PAGE`] live rows + tombstones) of terminal
    /// interactions strictly after a per-follower delivery cursor, in
    /// ASCENDING `(updated_at, id)` order. The lag recovery drains page after
    /// page (each call advances past the previous page's last row) until a
    /// short page, so a burst larger than one page cannot permanently skip the
    /// older rows. The cursor is the opaque token [`settlement_cursor_of`] —
    /// a COMPOSITE `(updated_at, id)` key so rows one transaction settled in
    /// the same millisecond cannot be skipped. With no cursor (a follower that
    /// has never passed a row) the drain starts at the OLDEST terminal row and
    /// walks forward; an older lost settlement is still authoritative.
    pub async fn invalidated_interactions_after(
        &self,
        cursor: Option<String>,
    ) -> Result<Vec<(String, String, String, String)>, StoreError> {
        self.invalidated_interactions_page(None, cursor, SETTLEMENT_LAG_PAGE)
            .await
    }

    /// Build the opaque lag-recovery cursor for a delivered row.
    #[must_use]
    pub(crate) fn settlement_cursor_of(updated_at: &str, id: &str) -> String {
        format!("{updated_at}{SETTLEMENT_CURSOR_SEP}{id}")
    }

    /// Advance `cursor` past a delivered/paged row using the composite
    /// `(updated_at, id)` ordering; `None` starts at the row.
    #[must_use]
    pub(crate) fn settlement_max_cursor(
        cursor: Option<&str>,
        updated_at: &str,
        id: &str,
    ) -> String {
        let token = Self::settlement_cursor_of(updated_at, id);
        match cursor {
            Some(prev) if prev > token.as_str() => prev.to_owned(),
            _ => token,
        }
    }

    /// Parse an opaque cursor into its `(updated_at, id)` parts. A malformed
    /// token (never one we issued) restarts the drain from the oldest row
    /// rather than silently filtering everything out.
    fn parse_settlement_cursor(cursor: Option<&str>) -> (Option<&str>, Option<&str>) {
        match cursor.map(|token| token.split_once(SETTLEMENT_CURSOR_SEP)) {
            Some(Some((updated_at, id))) if !updated_at.is_empty() && !id.is_empty() => {
                (Some(updated_at), Some(id))
            }
            _ => (None, None),
        }
    }

    /// Shared bounded page over live invalidated rows UNION ALL tombstones.
    /// The reconnect snapshot is bounded by a recent WINDOW; the lag cursor
    /// path is windowless (an older lost settlement is still authoritative)
    /// and pages forward from the cursor. Rows are always ASCENDING so a
    /// multi-page drain reaches the oldest missed rows; `limit` bounds rows
    /// AND tombstones together.
    async fn invalidated_interactions_page(
        &self,
        window_mins: Option<i64>,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<Vec<(String, String, String, String)>, StoreError> {
        let limit = i64::from(limit);
        self.run_named("invalidated_interactions_page", move |conn| {
            // r6 item 2: composite cursor + bounded LIMIT; always oldest-first
            // so repeated pages drain the ENTIRE backlog. A token present but
            // unparseable behaves like "no durable position".
            let (cursor_ts, cursor_id) = Self::parse_settlement_cursor(cursor.as_deref());
            let sql = "SELECT instance_id, id, COALESCE(
                    json_extract(payload_json, '$.payload.reasonCode'),
                    json_extract(payload_json,
                        '$.payload.entity.resolution.value.reason'),
                    json_extract(payload_json,
                        '$.payload.interaction.resolution.value.reason'),
                    'generation-ended'),
                    updated_at
                 FROM interactions
                 WHERE state = 'invalidated'
                   AND (?1 IS NULL OR julianday(updated_at) >= julianday('now','-' || ?1 || ' minutes'))
                   AND (?2 IS NULL OR (updated_at, id) > (?2, ?3))
                 UNION ALL
                 SELECT instance_id, id,
                        CASE WHEN reason = '' THEN 'generation-ended' ELSE reason END,
                        updated_at
                 FROM interaction_tombstones
                 WHERE state = 'invalidated'
                   AND (?1 IS NULL OR julianday(updated_at) >= julianday('now','-' || ?1 || ' minutes'))
                   AND (?2 IS NULL OR (updated_at, id) > (?2, ?3))
                 ORDER BY updated_at ASC, id ASC
                 LIMIT ?4";
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(
                params![window_mins, cursor_ts, cursor_id, limit],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// One interaction by id.
    pub async fn get_interaction(
        &self,
        interaction_id: String,
    ) -> Result<Option<InteractionRecord>, StoreError> {
        self.run_named("get_interaction", move |conn| {
            load_interaction(conn, &interaction_id)
        })
        .await
    }

    /// c-cardsettle r2 item 2: terminal state retained for an interaction whose
    /// owning instance was deleted. Consulted by the answer path when no live
    /// row exists, so a late answer after delete is rejected by state instead
    /// of fanning `interaction.answer` out to every connected Node.
    pub async fn get_interaction_tombstone(
        &self,
        interaction_id: String,
    ) -> Result<Option<InteractionTombstone>, StoreError> {
        self.run_named("get_interaction_tombstone", move |conn| {
            conn.query_row(
                "SELECT id, instance_id, host_id, state FROM interaction_tombstones WHERE id = ?1",
                params![interaction_id],
                |row| {
                    Ok(InteractionTombstone {
                        interaction_id: row.get(0)?,
                        instance_id: row.get(1)?,
                        host_id: row.get(2)?,
                        state: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
        })
        .await
    }

    /// Mirror a successful Node answer ACK; never decide a winner in the Hub.
    pub async fn record_interaction_answer(
        &self,
        interaction_id: String,
    ) -> Result<(), StoreError> {
        self.run_named("record_interaction_answer", move |conn| {
            conn.execute("UPDATE interactions SET state = 'answer-committed', updated_at = ?1 WHERE id = ?2 AND state = 'pending'", params![now_rfc3339(), interaction_id])?;
            Ok(())
        }).await
    }

    /// Bounded journal window on the reader pool.
    ///
    /// Selects rows in `(after_seq, high]`, where `high` is
    /// `before_seq.clamp(..=durable_seq)` when given else the durable tail, and
    /// returns at most [`JOURNAL_WINDOW_ROWS`] / [`JOURNAL_WINDOW_BYTES`] of
    /// the NEWEST rows in that range. The default call (no `before_seq`) is the
    /// durable tail, so a screen read costs O(window) rather than O(journal)
    /// (hub-store-1).
    ///
    /// The result says what it covers: [`JournalPage::from_seq`] is the floor
    /// and [`JournalPage::reached_after_seq`] is false when rows below the
    /// floor were cut. Re-paging the SAME `after_seq` returns the SAME tail; to
    /// walk the cut-away older history a caller descends with `before_seq =
    /// from_seq - 1` until `reached_after_seq` is true. There is no ascending
    /// `limit`/`offset` on this endpoint.
    ///
    /// Durable seq and the window are read inside one deferred transaction, so
    /// they share one WAL snapshot: an append landing between the two reads
    /// cannot make the window carry an event past the durable seq returned
    /// beside it (which would make a cursoring caller re-deliver).
    pub async fn read_journal(
        &self,
        instance_id: String,
        after_seq: i64,
        before_seq: Option<i64>,
    ) -> Result<JournalPage, StoreError> {
        self.read("read_journal", move |conn| {
            // Deferred BEGIN over the &Connection: the pool hands out shared
            // refs, and this conn is used on one blocking thread at a time.
            // The tx rolls back on error via the Transaction guard, so a failed
            // window cannot leave the pooled connection inside a transaction.
            let tx = conn.unchecked_transaction()?;
            let durable = load_instance(&tx, &instance_id)?
                .map(|i| i.durable_seq.parse::<i64>().unwrap_or(0))
                .unwrap_or(0);
            // Inclusive high bound; NULL means the durable tail.
            let high: Option<i64> = before_seq.map(|before| before.min(durable));
            let events = {
                // Walk back from the high end so the byte cap keeps the newest
                // rows; the rows come out descending and are reversed once cut.
                let mut stmt = tx.prepare(
                    "SELECT seq, event_id, payload_json, observed_at FROM journal
                     WHERE instance_id = ?1 AND seq > ?2 AND (?3 IS NULL OR seq <= ?3)
                     ORDER BY seq DESC LIMIT ?4",
                )?;
                let mut rows =
                    stmt.query(params![instance_id, after_seq, high, JOURNAL_WINDOW_ROWS])?;
                let mut events: Vec<JournalRecord> = Vec::new();
                let mut bytes = 0usize;
                while let Some(row) = rows.next()? {
                    let payload: String = row.get(2)?;
                    // Always keep the first (newest) row: a single event larger
                    // than the whole budget must still be readable.
                    if !events.is_empty()
                        && bytes.saturating_add(payload.len()) > JOURNAL_WINDOW_BYTES
                    {
                        break;
                    }
                    bytes = bytes.saturating_add(payload.len());
                    events.push(JournalRecord {
                        instance_id: instance_id.clone(),
                        seq: row.get(0)?,
                        event_id: row.get(1)?,
                        event: serde_json::from_str(&payload).unwrap_or(Value::Null),
                        observed_at: row.get(3)?,
                    });
                }
                events.reverse();
                events
            };
            tx.commit()?;
            let from_seq = events.first().map(|event| event.seq);
            let reached_after_seq = match from_seq {
                // Empty window: nothing exists in (after_seq, high].
                None => true,
                // All queried rows are > after_seq by construction, so the floor
                // reaches the cursor exactly at after_seq + 1.
                Some(first) => first == after_seq + 1,
            };
            Ok(JournalPage {
                events,
                durable_seq: durable,
                from_seq,
                reached_after_seq,
            })
        })
        .await
    }

    /// Read the raw JSON of the instance's last `limit` journal events, in
    /// ascending seq order. Watch classification only needs the tail (the last
    /// assistant message / turn result), so this avoids re-reading a whole
    /// long-lived journal on every observation (watch-failed-1).
    pub async fn read_journal_tail(
        &self,
        instance_id: String,
        limit: i64,
    ) -> Result<Vec<Value>, StoreError> {
        self.read("read_journal_tail", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT payload_json FROM (
                    SELECT payload_json, seq FROM journal
                    WHERE instance_id = ?1 ORDER BY seq DESC LIMIT ?2
                 ) ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map(params![instance_id, limit], |row| {
                let payload: String = row.get(0)?;
                Ok(serde_json::from_str(&payload).unwrap_or(Value::Null))
            })?;
            let values = rows.collect::<Result<Vec<_>, _>>()?;
            Ok(values)
        })
        .await
    }

    /// Operator PATCH of labels / maxInstances / display name / provider
    /// binding / relay bind (does not mark online).
    #[allow(clippy::too_many_arguments)] // one flat PATCH; grouping hurts callers
    pub async fn patch_host(
        &self,
        host_id: String,
        name: Option<String>,
        labels: Option<Value>,
        max_instances: Option<i64>,
        provider_binding: Option<String>,
        launch_defaults: HostLaunchDefaultsPatch,
        relay_bind: Option<Option<remuda_protocol::HostRelayBind>>,
    ) -> Result<HostRecord, StoreError> {
        let HostLaunchDefaultsPatch {
            default_launch_args,
            claude_binary_path,
            default_tui,
        } = launch_defaults;
        self.run_named("patch_host", move |conn| {
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
                // Operator intent lives in its own column: `apply_inventory`
                // keeps overwriting `max_instances` from every Node hello, so
                // writing there would be reset on the next reconnect.
                conn.execute(
                    "UPDATE hosts SET max_instances_override = ?1 WHERE id = ?2",
                    params![max_instances, host_id],
                )?;
            }
            if let Some(provider_binding) = provider_binding {
                conn.execute(
                    "UPDATE hosts SET provider_binding = ?1 WHERE id = ?2",
                    params![provider_binding, host_id],
                )?;
            }
            // Two levels of Option: the outer is "did the PATCH mention this
            // field", the inner is "set it or clear it". Collapsing them would
            // leave no way to remove a default once set.
            if let Some(args) = default_launch_args {
                let encoded = args
                    .map(|args| serde_json::to_string(&args))
                    .transpose()
                    .map_err(|error| StoreError::Id(error.to_string()))?;
                conn.execute(
                    "UPDATE hosts SET default_launch_args = ?1 WHERE id = ?2",
                    params![encoded, host_id],
                )?;
            }
            if let Some(path) = claude_binary_path {
                let path = path.filter(|value| !value.trim().is_empty());
                conn.execute(
                    "UPDATE hosts SET claude_binary_path = ?1 WHERE id = ?2",
                    params![path, host_id],
                )?;
            }
            if let Some(tui) = default_tui {
                let encoded = tui
                    .map(|tui| serde_json::to_string(&tui))
                    .transpose()
                    .map_err(|error| StoreError::Id(error.to_string()))?;
                conn.execute(
                    "UPDATE hosts SET default_tui = ?1 WHERE id = ?2",
                    params![encoded, host_id],
                )?;
            }
            // D-047 Amendment A1: double Option like the launch defaults —
            // absent means "PATCH did not mention it", explicit null clears.
            if let Some(relay_bind) = relay_bind {
                let encoded = relay_bind
                    .map(|bind| serde_json::to_string(&bind))
                    .transpose()
                    .map_err(|error| StoreError::Id(error.to_string()))?;
                conn.execute(
                    "UPDATE hosts SET relay_bind_json = ?1 WHERE id = ?2",
                    params![encoded, host_id],
                )?;
            }
            load_host(conn, &host_id)?.ok_or_else(|| StoreError::Id("unknown host".into()))
        })
        .await
    }

    /// Instances that still occupy a concurrency slot.
    ///
    /// Only instances the Node has actually confirmed are live count
    /// (ready/running, plus the `preparing`/`starting`/`closing` transitions
    /// that hold a real slot). A `requested` row the Node never acknowledged is
    /// a Hub-side intent; counting those let stale creates wedge a host at
    /// `maxInstances` forever.
    pub async fn running_count(&self, host_id: String) -> Result<i64, StoreError> {
        self.run_named("running_count", move |conn| {
            conn.query_row(LIVE_INSTANCE_COUNT_SQL, params![host_id], |row| row.get(0))
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
        self.run_named("insert_fleet", move |conn| {
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
        self.run_named("get_fleet", move |conn| {
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
        self.read("list_devices", |conn| {
            let mut stmt = conn
                .prepare("SELECT id, name, kind, instance_id FROM devices ORDER BY created_at")?;
            let rows = stmt.query_map([], |row| {
                Ok(Device {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    instance_id: row.get(3)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
        .await
    }

    /// Remove a device row.
    pub async fn delete_device(&self, device_id: String) -> Result<bool, StoreError> {
        self.run_named("delete_device", move |conn| {
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
        self.run_named("insert_pair_code", move |conn| {
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
        self.run_named("consume_pair_code", move |conn| {
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

    /// List provider profiles (metadata only). `host_id` returns universal + that host's scoped rows.
    pub async fn list_providers(
        &self,
        host_id: Option<String>,
    ) -> Result<Vec<ProviderRecord>, StoreError> {
        self.run_named("list_providers", move |conn| {
            if let Some(host_id) = host_id {
                let host_scope = format!("host:{host_id}");
                let mut stmt = conn.prepare(
                    "SELECT id, name, kind, base_url, models_json, default_model, headers_json,
                            is_default, revision, secret_name, secret_last4, secret_fingerprint,
                            last_test_ok, last_test_at, last_test_message, created_at, updated_at, scope,
                            supply_json, delivery_json
                     FROM provider_profiles
                     WHERE scope = 'universal' OR scope = '' OR scope = ?1
                     ORDER BY is_default DESC, name COLLATE NOCASE ASC, id ASC",
                )?;
                let rows = stmt.query_map(params![host_scope], load_provider_row)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(StoreError::from)
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, name, kind, base_url, models_json, default_model, headers_json,
                            is_default, revision, secret_name, secret_last4, secret_fingerprint,
                            last_test_ok, last_test_at, last_test_message, created_at, updated_at, scope,
                            supply_json, delivery_json
                     FROM provider_profiles
                     ORDER BY is_default DESC, name COLLATE NOCASE ASC, id ASC",
                )?;
                let rows = stmt.query_map([], load_provider_row)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(StoreError::from)
            }
        })
        .await
    }

    /// Fetch one profile.
    pub async fn get_provider(&self, id: String) -> Result<Option<ProviderRecord>, StoreError> {
        self.run_named("get_provider", move |conn| load_provider(conn, &id))
            .await
    }

    /// The profile marked default gateway in `scope` (`universal` or `host:<id>`).
    pub async fn default_gateway(
        &self,
        scope: String,
    ) -> Result<Option<ProviderRecord>, StoreError> {
        self.run_named("default_gateway", move |conn| {
            let id: Option<String> = conn
                .query_row(
                    "SELECT id FROM provider_profiles
                     WHERE is_default = 1 AND kind = 'gateway' AND (scope = ?1 OR (?1 = 'universal' AND (scope = '' OR scope IS NULL)))
                     LIMIT 1",
                    params![scope],
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
        models: Vec<ProviderModel>,
        default_model: Option<String>,
        headers: BTreeMap<String, String>,
        default_gateway: bool,
        scope: String,
        secret_name: Option<String>,
        secret_last4: Option<String>,
        secret_fingerprint: Option<String>,
        supply: remuda_protocol::SupplyProfile,
        delivery: remuda_protocol::ProviderDelivery,
    ) -> Result<ProviderRecord, StoreError> {
        self.run_named("insert_provider", move |conn| {
            let now = now_rfc3339();
            if default_gateway {
                conn.execute(
                    "UPDATE provider_profiles SET is_default = 0 WHERE is_default = 1 AND scope = ?1",
                    params![scope],
                )?;
            }
            conn.execute(
                "INSERT INTO provider_profiles
                 (id, name, kind, base_url, models_json, default_model, headers_json,
                  is_default, revision, secret_name, secret_last4, secret_fingerprint,
                  created_at, updated_at, scope, supply_json, delivery_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11, ?12, ?12, ?13, ?14, ?15)",
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
                    scope,
                    serde_json::to_string(&supply)?,
                    serde_json::to_string(&delivery)?,
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
        models: Option<Vec<ProviderModel>>,
        default_model: Option<Option<String>>,
        headers: Option<BTreeMap<String, String>>,
        default_gateway: Option<bool>,
        scope: Option<String>,
        secret_name: Option<Option<String>>,
        secret_last4: Option<Option<String>>,
        secret_fingerprint: Option<Option<String>>,
        supply: Option<remuda_protocol::SupplyProfile>,
        delivery: Option<remuda_protocol::ProviderDelivery>,
    ) -> Result<ProviderRecord, StoreError> {
        self.run_named("update_provider", move |conn| {
            let existing = load_provider(conn, &id)?
                .ok_or_else(|| StoreError::Id("unknown provider".into()))?;
            let next_scope = scope.clone().unwrap_or_else(|| existing.scope.clone());
            let next_default = default_gateway.unwrap_or(existing.default_gateway);
            if next_default {
                conn.execute(
                    "UPDATE provider_profiles SET is_default = 0 WHERE is_default = 1 AND scope = ?1 AND id != ?2",
                    params![next_scope, id],
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
            if let Some(scope) = scope {
                conn.execute(
                    "UPDATE provider_profiles SET scope = ?1 WHERE id = ?2",
                    params![scope, id],
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
            if let Some(supply) = supply {
                conn.execute(
                    "UPDATE provider_profiles SET supply_json = ?1 WHERE id = ?2",
                    params![serde_json::to_string(&supply)?, id],
                )?;
            }
            if let Some(delivery) = delivery {
                conn.execute(
                    "UPDATE provider_profiles SET delivery_json = ?1 WHERE id = ?2",
                    params![serde_json::to_string(&delivery)?, id],
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
        self.run_named("record_provider_test", move |conn| {
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

    /// Replace a profile's declared/observed supply envelope; §4.2.
    ///
    /// Used by `PATCH …/supply` (user declaration) and by the feedback loop
    /// (429 / structured rate-limit frames). Bumps revision like other
    /// profile metadata edits.
    pub async fn update_provider_supply(
        &self,
        id: String,
        supply: remuda_protocol::SupplyProfile,
    ) -> Result<ProviderRecord, StoreError> {
        self.run_named("update_provider_supply", move |conn| {
            if load_provider(conn, &id)?.is_none() {
                return Err(StoreError::Id("unknown provider".into()));
            }
            let encoded = serde_json::to_string(&supply)?;
            let now = now_rfc3339();
            conn.execute(
                "UPDATE provider_profiles SET supply_json = ?1, revision = revision + 1, updated_at = ?2
                 WHERE id = ?3",
                params![encoded, now, id],
            )?;
            load_provider(conn, &id)?.ok_or_else(|| StoreError::Id("unknown provider".into()))
        })
        .await
    }

    /// Read every live instance (same lifecycle set as host counts) for
    /// supply concurrency accounting.
    pub async fn list_live_instances(&self) -> Result<Vec<InstanceRecord>, StoreError> {
        self.run_named("list_live_instances", |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM instances
                 WHERE lifecycle IN
                    ('preparing', 'starting', 'ready', 'running', 'closing', 'reconciling')",
            )?;
            let ids: Vec<String> = stmt
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            ids.into_iter()
                .map(|id| {
                    load_instance(conn, &id)?
                        .ok_or_else(|| StoreError::Id("instance missing during live list".into()))
                })
                .collect()
        })
        .await
    }

    /// Append one placement decision to the audit ledger; §5.6.
    pub async fn insert_placement_ledger(
        &self,
        instance_id: Option<String>,
        project_id: Option<String>,
        profile_id: Option<String>,
        model_id: Option<String>,
        host_id: Option<String>,
        decision: Value,
    ) -> Result<(), StoreError> {
        self.run_named("insert_placement_ledger", move |conn| {
            let now = now_rfc3339();
            let encoded = serde_json::to_string(&decision)?;
            conn.execute(
                "INSERT INTO placement_ledger
                    (instance_id, project_id, profile_id, model_id, host_id, decision_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![instance_id, project_id, profile_id, model_id, host_id, encoded, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Recent placement ledger rows (newest first); tests/support/CLI read.
    pub async fn list_placement_ledger(&self, limit: i64) -> Result<Vec<Value>, StoreError> {
        self.run_named("list_placement_ledger", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT instance_id, project_id, profile_id, model_id, host_id, decision_json, created_at
                 FROM placement_ledger ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                let decision_json: String = row.get(5)?;
                let mut decision: Value =
                    serde_json::from_str(&decision_json).unwrap_or_else(|_| json!({}));
                if let Some(obj) = decision.as_object_mut() {
                    if let Ok(v) = row.get::<_, Option<String>>(0) {
                        obj.insert("instanceId".into(), json!(v));
                    }
                    if let Ok(v) = row.get::<_, Option<String>>(1) {
                        obj.insert("projectId".into(), json!(v));
                    }
                    if let Ok(v) = row.get::<_, Option<String>>(2) {
                        obj.insert("profileId".into(), json!(v));
                    }
                    if let Ok(v) = row.get::<_, Option<String>>(3) {
                        obj.insert("modelId".into(), json!(v));
                    }
                    if let Ok(v) = row.get::<_, Option<String>>(4) {
                        obj.insert("hostId".into(), json!(v));
                    }
                    let created_at: String = row.get(6)?;
                    obj.insert("placedAt".into(), json!(created_at));
                }
                Ok(decision)
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
        .await
    }

    /// Delete a profile row. Caller deletes the vault entry.
    pub async fn delete_provider(&self, id: String) -> Result<Option<ProviderRecord>, StoreError> {
        self.run_named("delete_provider", move |conn| {
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
                last_test_ok, last_test_at, last_test_message, created_at, updated_at, scope,
                supply_json, delivery_json
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
    let models = provider_models::parse_models_json(&models_json);
    let headers: BTreeMap<String, String> = serde_json::from_str(&headers_json).unwrap_or_default();
    let secret_name: Option<String> = row.get(9)?;
    let last_test_ok: Option<i64> = row.get(12)?;
    let scope: String = row
        .get::<_, Option<String>>(17)?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "universal".into());
    let supply_json: String = row
        .get::<_, Option<String>>(18)?
        .unwrap_or_else(|| "{}".into());
    let supply = serde_json::from_str(&supply_json).unwrap_or_default();
    let delivery: remuda_protocol::ProviderDelivery = row
        .get::<_, Option<String>>(19)?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
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
        scope,
        supply,
        delivery,
    })
}

/// Open one read-only connection for the reader pool.
///
/// `SQLITE_OPEN_READ_ONLY` is the guarantee, not a convention: a read job
/// cannot write even by accident, so nothing on this path can corrupt the
/// single-writer discipline. No schema work and no `journal_mode` change — the
/// writer owns both, and WAL is a property of the database file.
fn open_reader(path: &Path) -> Result<Connection, rusqlite::Error> {
    let started = Instant::now();
    loop {
        match try_open_reader(path) {
            Ok(conn) => return Ok(conn),
            Err(err) if sqlite_is_busy(&err) && started.elapsed() < BUSY_WAIT => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }
}

fn try_open_reader(path: &Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_WAIT)?;
    conn.pragma_update(None, "busy_timeout", BUSY_WAIT.as_millis() as i64)?;
    Ok(conn)
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

/// Writer-side connection opener: sets WAL, the busy timeout, and runs the
/// schema batch below.
///
/// HUB-LOCK SITE (spec only — `docs/design/hub-topology.md` §3): before a
/// caller reaches this opener through `Store::open`, the implementation
/// batch must hold a non-blocking exclusive advisory flock on
/// `<data_dir>/hub.lock` (acquired after creating `data_dir` and before any
/// SQLite connection opens). Failure to acquire it must refuse startup
/// fail-closed instead of letting a second Hub process run its own writer
/// thread and schema against this directory; the fd is held for the Store's
/// life and released on drop/exit by the kernel — a stale lock file is
/// harmless and must never be deleted as a "fix". No lock exists today;
/// this comment changes no behavior.
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
            hostname TEXT,
            os TEXT
        );
        CREATE TABLE IF NOT EXISTS objects (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            media_type TEXT NOT NULL,
            stored_name TEXT NOT NULL,
            original_name TEXT,
            kind TEXT NOT NULL DEFAULT 'image',
            digest TEXT NOT NULL,
            byte_len INTEGER NOT NULL,
            bytes BLOB NOT NULL,
            created_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            anchor INTEGER
        );
        CREATE UNIQUE INDEX IF NOT EXISTS objects_instance_digest
            ON objects(instance_id, digest);
        CREATE INDEX IF NOT EXISTS objects_expires_at ON objects(expires_at);
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
            last_error TEXT,
            configure_seq INTEGER NOT NULL DEFAULT 0
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
            updated_at TEXT NOT NULL,
            settlement_outcome TEXT,
            settlement_reason TEXT,
            settlement_http_status INTEGER,
            settlement_http_body TEXT,
            initiator_instance_id TEXT,
            initiator_lineage_id TEXT,
            initiator_generation INTEGER,
            initiator_device_id TEXT
        );
        CREATE TABLE IF NOT EXISTS journal (
            instance_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            event_id TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            observed_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, seq)
        );
        CREATE TABLE IF NOT EXISTS deleted_instances (
            instance_id TEXT PRIMARY KEY,
            deleted_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS audit_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id TEXT NOT NULL,
            action TEXT NOT NULL,
            subject TEXT,
            detail_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS audit_log_subject ON audit_log(subject);
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
        CREATE TABLE IF NOT EXISTS enroll_tokens (
            id TEXT PRIMARY KEY,
            token_hash TEXT NOT NULL,
            token_prefix TEXT,
            created_by TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            used INTEGER NOT NULL DEFAULT 0,
            used_at TEXT,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS enroll_tokens_prefix ON enroll_tokens(token_prefix);
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
        -- c-cardsettle r2 item 2: terminal state of interactions whose
        -- instance was deleted. The live rows go away with the instance, but a
        -- late answer must still get the state-derived rejection (invalidated
        -- -> 404, expired -> 410, answered/resolved -> 409) instead of a
        -- missing-row miss that fans interaction.answer out to every Node.
        -- Retention: the LIFE of the database, same policy as
        -- deleted_instances. Rows hold no payload (id/instance/host/state/
        -- timestamps only, tens of bytes each), a stale link may answer at any
        -- time, and pruning is exactly what would re-open the all-Node fan-out
        -- hole; so there is deliberately no late-answer TTL prune.
        CREATE TABLE IF NOT EXISTS interaction_tombstones (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            state TEXT NOT NULL,
            reason TEXT NOT NULL DEFAULT 'generation-ended',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
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
        CREATE TABLE IF NOT EXISTS passkeys (
            id TEXT PRIMARY KEY,
            credential_id TEXT NOT NULL,
            public_key TEXT NOT NULL,
            counter INTEGER NOT NULL DEFAULT 0,
            transports TEXT NOT NULL DEFAULT 'null',
            name TEXT NOT NULL,
            aaguid TEXT,
            created_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_used_at TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS passkeys_credential_id
            ON passkeys(credential_id);
        CREATE TABLE IF NOT EXISTS project_route_overrides (
            project_id TEXT PRIMARY KEY,
            doc_json TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS worktree_leases (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL,
            host_id TEXT NOT NULL,
            workspace_id TEXT NOT NULL,
            dir_key TEXT NOT NULL,
            worktree_name TEXT,
            branch TEXT,
            project_id TEXT,
            refcount INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL,
            holder_instance_id TEXT,
            task_ids_json TEXT NOT NULL DEFAULT '[]',
            base_oid TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            released_at TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS worktree_leases_dir
            ON worktree_leases(host_id, workspace_id, dir_key);
        CREATE INDEX IF NOT EXISTS worktree_leases_holder
            ON worktree_leases(holder_instance_id);
        CREATE INDEX IF NOT EXISTS worktree_leases_state
            ON worktree_leases(host_id, workspace_id, state);
        ",
    )?;
    ensure_column(&conn, "devices", "kind", "TEXT NOT NULL DEFAULT 'human'")?;
    ensure_column(&conn, "devices", "instance_id", "TEXT")?;
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
    ensure_column(&conn, "hosts", "os", "TEXT")?;
    ensure_column(
        &conn,
        "hosts",
        "provider_binding",
        "TEXT NOT NULL DEFAULT 'auto'",
    )?;
    // Per-host launch defaults. Nullable like `hostname`: absent means "no
    // default", which is different from "an empty arg list".
    ensure_column(&conn, "hosts", "default_launch_args", "TEXT")?;
    ensure_column(&conn, "hosts", "claude_binary_path", "TEXT")?;
    ensure_column(&conn, "hosts", "default_tui", "TEXT")?;
    ensure_column(
        &conn,
        "provider_profiles",
        "scope",
        "TEXT NOT NULL DEFAULT 'universal'",
    )?;
    // Declared supply + observed window state (coordinator batch 3, §4.2).
    ensure_column(
        &conn,
        "provider_profiles",
        "supply_json",
        "TEXT NOT NULL DEFAULT '{}'",
    )?;
    // D-047: per-profile delivery (`{mode, route, viaHostId}`); '{}' parses as
    // the direct/auto default through the serde defaults.
    ensure_column(
        &conn,
        "provider_profiles",
        "delivery_json",
        "TEXT NOT NULL DEFAULT '{}'",
    )?;
    // D-047 Amendment A1: operator-configured non-loopback relay bind.
    ensure_column(&conn, "hosts", "relay_bind_json", "TEXT")?;
    // D-047: the observed API route, written only from the Node create echo.
    ensure_column(&conn, "instances", "api_route_json", "TEXT")?;
    ensure_column(&conn, "instances", "last_error", "TEXT")?;
    ensure_column(&conn, "instances", "mode", "TEXT")?;
    ensure_column(&conn, "instances", "promoted_at", "TEXT")?;
    // D-028 §1.0 rule 4. Stored rather than inferred: §1.0 rule 2 makes
    // promotion the only detection path, so `mode == promoted` stopped meaning
    // "a human typed it" once Remuda-launched agents began promoting too.
    ensure_column(&conn, "instances", "launched_by", "TEXT")?;
    // Operator ceiling survives Node hello/heartbeat inventory and Hub restarts.
    ensure_column(&conn, "hosts", "max_instances_override", "INTEGER")?;
    // Delegation-tree columns; design §2.5 (additive, same migration shape).
    ensure_column(&conn, "instances", "role", "TEXT")?;
    ensure_column(&conn, "instances", "scope_json", "TEXT")?;
    ensure_column(
        &conn,
        "instances",
        "grants_json",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    ensure_column(&conn, "instances", "task_id", "TEXT")?;
    // D-055 round 2: authoritative count of committed configure spec merges.
    ensure_column(
        &conn,
        "instances",
        "configure_seq",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // D-057 (ma-lineage): every instance belongs to a lineage (its own id by
    // default); a continuity lineage's chapters carry a generation and cause.
    // `fenced_at` marks a chapter whose authority moved to its successor, and
    // `restart_json` is the C1 restart policy set at create time.
    ensure_column(&conn, "instances", "lineage_id", "TEXT")?;
    ensure_column(
        &conn,
        "instances",
        "generation",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    ensure_column(&conn, "instances", "chapter_cause", "TEXT")?;
    ensure_column(&conn, "instances", "fenced_at", "TEXT")?;
    ensure_column(&conn, "instances", "restart_json", "TEXT")?;
    // D-057 §7.1 (ma-initiator): every Agent-initiated command row carries
    // the Hub-stamped initiator and the authenticating device id. The device
    // id stays Hub-only and is never sent to Nodes. NULL for Human/Bot work.
    ensure_column(&conn, "commands", "initiator_instance_id", "TEXT")?;
    ensure_column(&conn, "commands", "initiator_lineage_id", "TEXT")?;
    ensure_column(&conn, "commands", "initiator_generation", "INTEGER")?;
    ensure_column(&conn, "commands", "initiator_device_id", "TEXT")?;
    // ma-lineage round 2: an immutable timestamp for the chapter's real end
    // event (a transition into exited/failed/closed), kept separate from the
    // mutable `updated_at`. Written once by [`stamp_ended_at`].
    ensure_column(&conn, "instances", "ended_at", "TEXT")?;
    // ma-lineage r4 item 3 + r6 item 7: the ended_at backfill runs ONCE,
    // guarded by a schema PRAGMA marker, not on every Store::open for every
    // evidence-less terminal row (that set only grows).
    let backfill_version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if backfill_version < 2 {
        // r6 item 3(b) ONE-TIME legacy fallback (guarded by PRAGMA
        // user_version): evidence-less historical rows that already reached a
        // live lifecycle, and old 'host-lost'-marked rows, are stamped from
        // updated_at — they are genuinely dead historical rows from before
        // the host-lost-is-contact-loss semantics. The new r6 marker keeps
        // later host-lost rows potentially-live until a nodeEpoch change.
        // Rows that never reached live (requested / attested launch failures)
        // stay NULL. (The journal-event backfill below stays unguarded: it
        // only ever stamps rows WITH a qualifying end event and must still see
        // journal events written after an earlier open — r6 item 7.)
        conn.execute(
            "UPDATE instances SET ended_at = updated_at
             WHERE ended_at IS NULL
               AND lifecycle IN ('exited', 'failed')
               AND (
                    last_error = 'host-lost'
                    OR (last_error IS NULL
                        AND lifecycle = 'exited'
                        AND EXISTS (
                            SELECT 1 FROM journal
                             WHERE journal.instance_id = instances.id
                               AND json_extract(journal.payload_json,
                                   '$.payload.state') IN ('ready','running')
                        ))
               )",
            [],
        )?;
        conn.execute_batch("PRAGMA user_version = 2;")?;
    }
    // ma-lineage r4 item 3: backfill `ended_at` from durable, classifier-
    // qualified process-end EVENTS in each row's journal — never blindly from
    // updated_at. A later return-to-live event after the end vetoes it; a row
    // with no qualifying event keeps NULL. Idempotent and evidence-only, so it
    // safely re-runs (it can stamp a journal end written after an earlier
    // open); only the legacy fallback above is one-time (r6 item 7).
    backfill_ended_at_from_journal(&conn)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    // Rows written before the column existed are each their own lineage.
    conn.execute(
        "UPDATE instances SET lineage_id = id WHERE lineage_id IS NULL",
        [],
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS lineages (
            lineage_id TEXT PRIMARY KEY,
            current_instance_id TEXT NOT NULL,
            generation INTEGER NOT NULL,
            state TEXT NOT NULL,
            paused_by_json TEXT,
            paused_at TEXT,
            restart_json TEXT,
            origin_spec_ref TEXT,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS lineages_current ON lineages(current_instance_id);",
    )?;
    // D-057 §5 (ma-lineage round 2): rows written before ma-lineage got their
    // lineage_id stamped above but no lineage ROW, so a holder's resume fell
    // back to the plain D-026 path and dropped every continuity property.
    // Backfill one lineage per existing continuity ROOT — an instance that
    // holds any grant or carries a restart policy and is itself the FIRST
    // chapter of its lineage. Only true roots qualify: a row with no stamped
    // lineage_id (a pre-ma-lineage row) or one whose lineage_id equals its own
    // id (a lineage root created by current code). A SUCCESSOR chapter carries
    // lineage_id = its predecessor's id together with copied grants/restart;
    // selecting it by its own id and inserting a lineage keyed by that id
    // would manufacture a phantom second lineage for one continuation. The
    // NOT EXISTS guard therefore checks the STAMPED lineage id. Idempotent on
    // every open.
    conn.execute(
        "INSERT INTO lineages
            (lineage_id, current_instance_id, generation, state,
             paused_by_json, paused_at, restart_json, origin_spec_ref, updated_at)
         SELECT COALESCE(i.lineage_id, i.id), i.id, 1,
                CASE WHEN i.lifecycle IN ('requested','preparing','starting')
                     THEN 'starting' ELSE 'running' END,
                NULL, NULL, i.restart_json,
                json_object(
                    'origin', COALESCE(json_extract(i.spec_json, '$.origin'), 'agent'),
                    'spec', json(i.spec_json)
                ),
                i.updated_at
           FROM instances i
          WHERE (i.restart_json IS NOT NULL
                 OR (i.grants_json IS NOT NULL AND i.grants_json != '[]'))
            AND (i.lineage_id IS NULL OR i.lineage_id = i.id)
            AND NOT EXISTS (
                SELECT 1 FROM lineages l
                 WHERE l.lineage_id = COALESCE(i.lineage_id, i.id)
            )",
        [],
    )?;
    // Last `nodeEpoch` announced by this host, used to detect a Node restart.
    ensure_column(&conn, "hosts", "node_epoch", "TEXT")?;
    ensure_column(&conn, "hosts", "offline_since", "TEXT")?;
    ensure_column(&conn, "devices", "token_prefix", "TEXT")?;
    ensure_column(&conn, "hosts", "token_prefix", "TEXT")?;
    // 2026-09-15: [Image #n] anchor assigned by the send manifest.
    ensure_column(&conn, "objects", "anchor", "INTEGER")?;
    // D-027b (2026-09-15): arbitrary files carry their sanitised original
    // filename and an image/file kind; pre-D-027b rows were all images.
    ensure_column(&conn, "objects", "original_name", "TEXT")?;
    ensure_column(&conn, "objects", "kind", "TEXT NOT NULL DEFAULT 'image'")?;
    ensure_column(&conn, "pair_codes", "code_prefix", "TEXT")?;
    ensure_column(
        &conn,
        "pair_codes",
        "failed_attempts",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // Protocol §2.5: a command has only three progress states. A rejected
    // delivery is `settled` carrying settlement outcome `rejected` and the
    // Node's reason, never a fourth state. These two columns project that
    // settlement; they are NULL until the row settles.
    ensure_column(&conn, "commands", "settlement_outcome", "TEXT")?;
    ensure_column(&conn, "commands", "settlement_reason", "TEXT")?;
    // D-055 round 2: a non-replayable command replays its first attempt's
    // exact HTTP status and body; NULL for 200 rows and pre-round-2 data.
    ensure_column(&conn, "commands", "settlement_http_status", "INTEGER")?;
    ensure_column(&conn, "commands", "settlement_http_body", "TEXT")?;
    // c-cardsettle r5 item 8: tombstones retain the real invalidation reason
    // so reconnect/lag replay labels a demotion as agent-demoted, not
    // generation-ended.
    ensure_column(
        &conn,
        "interaction_tombstones",
        "reason",
        "TEXT NOT NULL DEFAULT 'generation-ended'",
    )?;
    // One-time cleanup of the round-2 shape: a failed delivery was briefly a
    // fourth `state='failed'` value held in a `reason` column. Fold any rows an
    // older build persisted into the §2.5 form (`settled` + a `rejected`
    // settlement carrying that reason), then drop the orphaned column. The
    // column drop also discards a stale `reason` a pre-fix mark_settled could
    // have left on a completed row. Bundled SQLite is >= 3.35 (DROP COLUMN).
    if column_exists(&conn, "commands", "reason")? {
        let migrated = conn.execute(
            "UPDATE commands
             SET state = 'settled', resolution = 'clear',
                 settlement_outcome = 'rejected', settlement_reason = reason
             WHERE state = 'failed'",
            [],
        )?;
        if migrated > 0 {
            tracing::info!(
                migrated,
                "folded legacy failed commands into rejected settlements"
            );
        }
        conn.execute("ALTER TABLE commands DROP COLUMN reason", [])?;
    }
    conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS devices_token_prefix ON devices(token_prefix) WHERE token_prefix IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS hosts_token_prefix ON hosts(token_prefix) WHERE token_prefix IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS pair_codes_prefix ON pair_codes(code_prefix) WHERE code_prefix IS NOT NULL;
        CREATE INDEX IF NOT EXISTS commands_instance ON commands(instance_id, created_at DESC);")?;
    crate::workspaces::migrate(&conn)?;
    crate::projects::migrate(&conn)?;
    crate::tasks::migrate(&conn)?;
    crate::supply::migrate(&conn)?;
    crate::workers::migrate(&conn)?;
    crate::gatequeue::migrate(&conn)?;
    crate::node_ops::migrate(&conn)?;
    crate::usage_store::migrate(&conn)?;
    migrate_provider_models(&conn)?;
    crate::store_tickets::migrate(&conn)?;
    dedup_duplicate_hosts(&conn)?;
    Ok(conn)
}

/// Rewrite legacy `["id", …]` catalogs as structured rows (all enabled).
///
/// Reads tolerate either shape, so this only makes the stored form uniform;
/// a row that cannot be parsed is left untouched rather than emptied.
fn migrate_provider_models(conn: &Connection) -> Result<(), rusqlite::Error> {
    let legacy: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT id, models_json FROM provider_profiles")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<(String, String)>, _>>()?
            .into_iter()
            .filter(|(_, raw)| provider_models::is_legacy_json(raw))
            .collect()
    };
    for (id, raw) in legacy {
        let models = provider_models::parse_models_json(&raw);
        let Ok(encoded) = serde_json::to_string(&models) else {
            continue;
        };
        conn.execute(
            "UPDATE provider_profiles SET models_json = ?1 WHERE id = ?2",
            params![encoded, id],
        )?;
    }
    Ok(())
}

/// §9.1: persist the transcript-observed effective model and its discovered
/// catalog onto the spec.
fn apply_effective_model_projection(
    conn: &Connection,
    instance_id: &str,
    effective: &Value,
    catalog: Option<&Value>,
) -> Result<(), StoreError> {
    let spec_raw: String = conn.query_row(
        "SELECT spec_json FROM instances WHERE id = ?1",
        params![instance_id],
        |row| row.get(0),
    )?;
    let mut spec: Value = serde_json::from_str(&spec_raw).unwrap_or(json!({}));
    if let Some(object) = spec.as_object_mut() {
        object.insert("modelEffective".into(), effective.clone());
        if let Some(catalog) = catalog {
            object.insert("modelCatalog".into(), catalog.clone());
        }
        conn.execute(
            "UPDATE instances SET spec_json = ?1 WHERE id = ?2",
            params![Value::Object(object.clone()).to_string(), instance_id],
        )?;
    }
    Ok(())
}

/// model-pin-1 §5.4: persist a recorded launch model-pin divergence onto the
/// instance spec, verbatim, so run details keeps the authoritative record
/// independently of the bounded journal tail window. Records accumulate
/// (a session can be re-launched); de-duped on the full record.
fn apply_model_pin_mismatch_projection(
    conn: &Connection,
    instance_id: &str,
    record: &Value,
) -> Result<(), StoreError> {
    let spec_raw: String = conn.query_row(
        "SELECT spec_json FROM instances WHERE id = ?1",
        params![instance_id],
        |row| row.get(0),
    )?;
    let mut spec: Value = serde_json::from_str(&spec_raw).unwrap_or(json!({}));
    if let Some(object) = spec.as_object_mut() {
        let existing = object
            .get("modelPinMismatches")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if existing.iter().any(|row| row == record) {
            return Ok(());
        }
        let mut next = existing;
        next.push(record.clone());
        object.insert("modelPinMismatches".into(), Value::Array(next));
        conn.execute(
            "UPDATE instances SET spec_json = ?1 WHERE id = ?2",
            params![Value::Object(object.clone()).to_string(), instance_id],
        )?;
    }
    Ok(())
}

/// §9.1: persist the transcript-observed effective effort onto the spec.
fn apply_effective_effort_projection(
    conn: &Connection,
    instance_id: &str,
    effective: &Value,
) -> Result<(), StoreError> {
    let spec_raw: String = conn.query_row(
        "SELECT spec_json FROM instances WHERE id = ?1",
        params![instance_id],
        |row| row.get(0),
    )?;
    let mut spec: Value = serde_json::from_str(&spec_raw).unwrap_or(json!({}));
    if let Some(object) = spec.as_object_mut() {
        object.insert("effortEffective".into(), effective.clone());
        conn.execute(
            "UPDATE instances SET spec_json = ?1 WHERE id = ?2",
            params![Value::Object(object.clone()).to_string(), instance_id],
        )?;
    }
    Ok(())
}

/// §5.1: persist the transcript-observed effective permission mode onto the
/// spec, mirroring `effortEffective`.
fn apply_effective_permission_projection(
    conn: &Connection,
    instance_id: &str,
    effective: &Value,
) -> Result<(), StoreError> {
    let spec_raw: String = conn.query_row(
        "SELECT spec_json FROM instances WHERE id = ?1",
        params![instance_id],
        |row| row.get(0),
    )?;
    let mut spec: Value = serde_json::from_str(&spec_raw).unwrap_or(json!({}));
    if let Some(object) = spec.as_object_mut() {
        object.insert("permissionEffective".into(), effective.clone());
        conn.execute(
            "UPDATE instances SET spec_json = ?1 WHERE id = ?2",
            params![Value::Object(object.clone()).to_string(), instance_id],
        )?;
    }
    Ok(())
}

fn apply_instance_projection(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
    seq: i64,
    now: &str,
    settlement: &mut Settlement,
) -> Result<(), StoreError> {
    let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
    let payload = event.get("payload").cloned().unwrap_or(Value::Null);
    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "effort"
        && let Some(effective) = payload.get("effective")
    {
        apply_effective_effort_projection(conn, instance_id, effective)?;
    }
    if kind == "model"
        && let Some(effective) = payload.get("effective")
    {
        // The selection path rides the model payload, not EffectiveModel;
        // merge it into the projected record so the listed/typed marker
        // survives a page reload. The driver stamps it on `effective`; the
        // older test/fake shape put it at the payload top level — accept both.
        let mut effective = effective.clone();
        if let Some(object) = effective.as_object_mut()
            && let Some(path) = object
                .get("selectionPath")
                .or_else(|| payload.get("selectionPath"))
                .cloned()
        {
            object.insert("selectionPath".into(), path);
        }
        apply_effective_model_projection(conn, instance_id, &effective, payload.get("catalog"))?;
    }
    if kind == "permission"
        && let Some(effective) = payload.get("effective")
    {
        apply_effective_permission_projection(conn, instance_id, effective)?;
    }
    let mut lifecycle: Option<&str> = None;
    // Shared-classifier process-end evidence carried by THIS event (entity or
    // native), used to stamp ended_at from the evidence timestamp. None for a
    // turn/configure/liveness event.
    let mut end_evidence: Option<remuda_protocol::process_end::ProcessEnd> = None;
    let mut last_error: Option<String> = None;
    if kind == "lifecycle" && payload_type == "entity" && payload_is_instance_entity(&payload) {
        let entity_state = payload.get("state").and_then(Value::as_str);
        // ma-lineage r4: an INSTANCE entity terminal is process end through
        // the shared classifier (exited → exited, failed → failed); `ready`
        // is return-to-live. Non-instance entities never reach this branch.
        if let Some(end) =
            remuda_protocol::process_end::entity_process_end(Some("instance"), entity_state)
        {
            lifecycle = Some(end.lifecycle());
            // Stamp ended_at from the EVENT timestamp (observedAt), not the
            // write clock — same as the native full-event classifier.
            let at = event
                .get("observedAt")
                .and_then(Value::as_str)
                .and_then(|raw| remuda_protocol::Timestamp::try_from(raw.to_owned()).ok());
            end_evidence = Some(end.with_at(at));
        } else if entity_state == Some("ready") {
            lifecycle = Some("ready");
        }
        // A reason/lastError is an error marker ONLY on a terminal entity; a
        // ready event's "driver-started" reason is a liveness note and must
        // never populate last_error (it would surface as a phantom failure).
        if end_evidence.is_some() {
            if let Some(reason) = payload.get("reasonCode").and_then(Value::as_str) {
                last_error = Some(reason.to_string());
            }
            if let Some(entity_error) = payload.pointer("/entity/lastError").and_then(Value::as_str)
            {
                last_error = Some(entity_error.to_string());
            }
        }
        conn.execute(
            "UPDATE instances SET spec_json = json_set(spec_json, '$.nativeSignalTier', ?1) WHERE id = ?2",
            params![payload.pointer("/entity/nativeRef/signalTier").and_then(Value::as_str), instance_id],
        )?;
    }
    if kind == "lifecycle" && payload_type == "native" {
        // model-pin-1 §5.4: project the launch divergence onto the instance
        // record so it survives beyond the bounded journal window. Verbatim
        // requested/observed + the event time; the client never recomputes.
        if payload.get("topic").and_then(Value::as_str) == Some("diagnostic")
            && payload.get("nativeName").and_then(Value::as_str) == Some("model_pin_mismatch")
            && let Some(related) = payload.get("relatedIds")
            && let (Some(requested), Some(observed)) = (
                related.get("requested").and_then(Value::as_str),
                related.get("observed").and_then(Value::as_str),
            )
        {
            let record = json!({
                "requested": requested,
                "observed": observed,
                "observedAt": event
                    .get("observedAt")
                    .and_then(Value::as_str)
                    .unwrap_or(now),
            });
            apply_model_pin_mismatch_projection(conn, instance_id, &record)?;
        }
        // D-025: a promoted terminal changes kind/mode on the Hub row too, so
        // the session list and header follow the agent the human started.
        let name = payload
            .get("nativeName")
            .and_then(Value::as_str)
            .unwrap_or("");
        if matches!(name, "agent_promoted" | "agent_demoted") {
            let related = payload.get("relatedIds");
            let promoted_kind = related
                .and_then(|ids| ids.get("kind"))
                .and_then(Value::as_str);
            let mode = related
                .and_then(|ids| ids.get("mode"))
                .and_then(Value::as_str)
                .unwrap_or(if name == "agent_promoted" {
                    "promoted"
                } else {
                    "native"
                });
            let promoted_at = (name == "agent_promoted")
                .then(|| {
                    related
                        .and_then(|ids| ids.get("promotedAt"))
                        .and_then(Value::as_str)
                })
                .flatten();
            if let Some(promoted_kind) = promoted_kind {
                // Settle provenance on the *first* promotion, using what the
                // row was before it: a `terminal` that becomes an agent is a
                // human typing into a shell, anything else is the launch
                // Remuda ran. Once written it never changes — a later
                // demote/repromote cycle must not rewrite where a session came
                // from. Mirrors the Node's own rule in `set_instance_promotion`
                // so the two cannot disagree.
                if name == "agent_promoted" {
                    conn.execute(
                        "UPDATE instances
                         SET launched_by = CASE WHEN kind = 'terminal' THEN 'user' ELSE 'remuda' END
                         WHERE id = ?1 AND launched_by IS NULL",
                        params![instance_id],
                    )?;
                }
                conn.execute(
                    "UPDATE instances SET kind = ?1, mode = ?2, promoted_at = ?3, updated_at = ?4
                     , spec_json = CASE WHEN ?6 = 'agent_demoted' THEN json_remove(spec_json, '$.nativeSignalTier') ELSE spec_json END
                     WHERE id = ?5",
                    params![promoted_kind, mode, promoted_at, now, instance_id, name],
                )?;
            }
        }
        let native_name = name.to_ascii_lowercase();
        // ma-lineage r4 item 1+2 (OA6): terminal ONLY from the SHARED
        // remuda_protocol::process_end classifier, driven by the REAL driver
        // event shapes (exact name/status). There is no name-substring or
        // bare-severity rule: a turn/configure/hook/diagnostic error on a live
        // process returns None and can never mark the row (or ended_at)
        // terminal. A clean SDK/print exit (topic=session, nativeName=session,
        // status=exited) and a clean PTY exit (native_exit, status=exited,
        // severity info) classify as `exited`, NOT failed. Subagent-scoped
        // native events never end the root.
        let subagent_scoped = native_payload_is_subagent(&payload);
        if !subagent_scoped
            && let Some(end) = remuda_protocol::process_end::process_end_event(event)
        {
            lifecycle = Some(end.lifecycle());
            end_evidence = Some(end);
        }
        let _ = native_name;
    }
    if let Some(lifecycle) = lifecycle {
        // c-cardsettle: capture the EFFECTIVE lifecycle before this write so
        // the settlement guard fires on the transition itself. This
        // projection recognises native terminals (nativeName "exit", severity
        // "error", prose failed statuses) that `apply_instance_lifecycle`'s
        // derivation does not — without the guard those used to write
        // lifecycle='failed' here while leaving the generation's pending cards
        // untouched.
        let previous_lifecycle: Option<String> = conn
            .query_row(
                "SELECT lifecycle FROM instances WHERE id = ?1",
                params![instance_id],
                |row| row.get(0),
            )
            .optional()?;
        conn.execute(
            "UPDATE instances SET durable_seq = ?1, lifecycle = ?2,
                    last_error = COALESCE(?3, last_error), updated_at = ?4
             WHERE id = ?5",
            params![seq, lifecycle, last_error, now, instance_id],
        )?;
        // ma-lineage r4 item 2: ended_at is stamped ONLY from the shared
        // classifier's process-end evidence (its `at`, or this write clock
        // when the event carried no timestamp); a `ready`/`running`/`starting`
        // event is return-to-live and clears it.
        apply_ended_at(conn, instance_id, lifecycle, end_evidence.as_ref(), now)?;
        // c-cardsettle: settle pending cards through the same transition guard
        // the projection write uses.
        settle_on_terminal_transition(
            conn,
            instance_id,
            previous_lifecycle.as_deref(),
            Some(lifecycle),
            now,
            settlement,
        )?;
        // D-027: a terminal instance can never consume a staged attachment
        // again, and the Node drops its own copy at the same point.
        if matches!(lifecycle, "exited" | "failed") {
            conn.execute(
                "DELETE FROM objects WHERE instance_id = ?1",
                params![instance_id],
            )?;
        }
    } else {
        conn.execute(
            "UPDATE instances SET durable_seq = ?1, updated_at = ?2 WHERE id = ?3",
            params![seq, now, instance_id],
        )?;
    }
    Ok(())
}

/// ma-lineage r4 item 3: backfill the immutable `ended_at` for rows written
/// before the column existed, from durable journal evidence only.
///
/// For every terminal row (`exited`/`failed`/`closed`) whose `ended_at` is
/// still NULL, walk its journal events in sequence:
///
/// * a SHARED-classifier process-end event (native session exit/launch
///   failure, or an instance entity exited/failed) records a candidate end,
///   the earliest one after the last observed liveness;
/// * ANY later event proving the process was alive again (a derived
///   ready/running/starting lifecycle) discards that candidate — later live
///   evidence wins, so an ambiguous "end" the process survived is never
///   stamped.
///
/// Only when the walk ends with a surviving candidate is `ended_at` stamped
/// with the END EVENT's own `observed_at`. A row with no qualifying event
/// stays NULL; nothing is copied from the mutable `updated_at`.
fn backfill_ended_at_from_journal(conn: &Connection) -> Result<(), StoreError> {
    let terminal: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM instances
             WHERE ended_at IS NULL
               AND lifecycle IN ('exited', 'failed', 'closed')",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for instance_id in terminal {
        let mut stmt = conn.prepare(
            "SELECT seq, payload_json, observed_at FROM journal
             WHERE instance_id = ?1
             ORDER BY seq ASC",
        )?;
        let events = stmt.query_map(params![instance_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        // The earliest qualifying end after the most recent return-to-live.
        let mut candidate: Option<String> = None;
        for row in events {
            let (_seq, payload_json, column_observed_at) = row?;
            let event: Value = match serde_json::from_str(&payload_json) {
                Ok(event) => event,
                Err(_) => continue,
            };
            // The event's own observedAt is the end time; the journal column
            // only carries the Hub write clock (append stamps `now`), so read
            // the JSON field first and fall back to the column.
            let observed_at = event
                .get("observedAt")
                .and_then(Value::as_str)
                .filter(|at| !at.is_empty())
                .map(str::to_string)
                .unwrap_or(column_observed_at);
            if journal_event_is_process_end(&event) {
                candidate.get_or_insert(observed_at);
            } else if journal_event_returns_to_live(&event) {
                candidate = None;
            }
        }
        if let Some(at) = candidate {
            stamp_ended_at(conn, &instance_id, &at)?;
        }
    }
    Ok(())
}

/// Whether a stored journal event is shared-classifier process-end evidence
/// (native lifecycle OR an instance entity state).
fn journal_event_is_process_end(event: &Value) -> bool {
    if remuda_protocol::process_end::process_end_event(event).is_some() {
        return true;
    }
    let payload = event.get("payload").unwrap_or(event);
    if payload.get("type").and_then(Value::as_str) != Some("entity") {
        return false;
    }
    // Same instance-entity rule as the projection/derivation (explicit
    // entityType=instance or a bare state-only driver shorthand).
    payload_is_instance_entity(payload)
        && remuda_protocol::process_end::entity_process_end(
            Some("instance"),
            payload.get("state").and_then(Value::as_str),
        )
        .is_some()
}

/// Whether a stored journal event proves the process was alive again AFTER a
/// candidate end (a derived non-terminal lifecycle: ready/running/starting).
fn journal_event_returns_to_live(event: &Value) -> bool {
    matches!(
        derive_instance_state(event).0,
        Some("ready" | "running" | "starting")
    )
}

/// Mirror the native session identity a Node reported onto the instance spec.
///
/// Resume needs the id `claude --resume` accepts, and the Hub only ever sees
/// Node journals. The driver reports it on a `session` (claude-print) or
/// `SessionStart` hook (claude-pty) native lifecycle, and the Node also
/// restates it on the Instance entity it journals (D-026).
fn apply_native_session_projection(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
    now: &str,
) -> Result<(), StoreError> {
    let Some((session_id, transcript_path)) = native_session_from_event(event) else {
        return Ok(());
    };
    let raw: String = match conn
        .query_row(
            "SELECT spec_json FROM instances WHERE id = ?1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?
    {
        Some(raw) => raw,
        None => return Ok(()),
    };
    let mut spec: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    let Some(object) = spec.as_object_mut() else {
        return Ok(());
    };
    let known_session = object
        .get("nativeSessionId")
        .and_then(Value::as_str)
        .is_some_and(|value| value == session_id);
    let known_transcript = match transcript_path.as_deref() {
        None => true,
        Some(path) => object
            .get("nativeTranscriptPath")
            .and_then(Value::as_str)
            .is_some_and(|value| value == path),
    };
    if known_session && known_transcript {
        return Ok(());
    }
    object.insert("nativeSessionId".into(), json!(session_id));
    if let Some(path) = transcript_path {
        object.insert("nativeTranscriptPath".into(), json!(path));
    }
    conn.execute(
        "UPDATE instances SET spec_json = ?1, updated_at = ?2 WHERE id = ?3",
        params![spec.to_string(), now, instance_id],
    )?;
    Ok(())
}

/// Session id + transcript path carried by one mirrored journal event.
fn native_session_from_event(event: &Value) -> Option<(String, Option<String>)> {
    if event.get("kind").and_then(Value::as_str) != Some("lifecycle") {
        return None;
    }
    let payload = event.get("payload")?;
    match payload.get("type").and_then(Value::as_str) {
        Some("native") => {
            if event.pointer("/source/driverKind").and_then(Value::as_str) == Some("shell-pty")
                && event.pointer("/source/channel").and_then(Value::as_str) == Some("hook")
            {
                return None;
            }
            let topic = payload.get("topic").and_then(Value::as_str).unwrap_or("");
            let name = payload
                .get("nativeName")
                .and_then(Value::as_str)
                .unwrap_or("");
            let transcript = payload
                .pointer("/relatedIds/transcriptPath")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            // `nativeId` is only a session on these two events; claude-print's
            // hook lifecycles put a hook id there (D-026).
            let carries_session = match topic {
                "session" => name == "session",
                "hook" => name == "SessionStart" && transcript.is_some(),
                _ => false,
            };
            if !carries_session {
                return None;
            }
            let session = payload
                .pointer("/nativeId/value")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())?;
            Some((session.to_string(), transcript))
        }
        Some("entity") => {
            if payload.get("entityType").and_then(Value::as_str) != Some("instance") {
                return None;
            }
            let session = payload
                .pointer("/entity/nativeRef/sessionId/value")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())?;
            let transcript = payload
                .pointer("/entity/nativeRef/transcript/value/sourcePath")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            Some((session.to_string(), transcript))
        }
        _ => None,
    }
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
            // A journaled accept is the Node's durable word (§2.5) and moves a
            // still-`queued` row (resolution `unknown` / `reconciling`) to
            // accepted. A settled row is terminal and never regresses.
            conn.execute(
                "UPDATE commands
                 SET state = 'accepted', resolution = 'clear',
                     settlement_outcome = NULL, settlement_reason = NULL, updated_at = ?1
                 WHERE id = ?2 AND state = 'queued'",
                params![now, command_id],
            )?;
        }
        "settled" => {
            // The Node's Command entity carries its §2.5 settlement as
            // `{state:"known", value:{outcome, error}}`. Only accept an outcome
            // from the protocol vocabulary; a normal completion without one is
            // `completed`. The rejection reason rides `error.message`.
            let outcome = payload
                .pointer("/entity/settlement/value/outcome")
                .and_then(Value::as_str)
                .filter(|outcome| {
                    serde_json::from_value::<SettlementOutcome>(json!(outcome)).is_ok()
                })
                .unwrap_or("completed");
            let reason = payload
                .pointer("/entity/settlement/value/error/message")
                .and_then(Value::as_str)
                .filter(|_| outcome == "rejected")
                .map(str::to_owned);
            conn.execute(
                "UPDATE commands
                 SET state = 'settled', resolution = 'clear',
                     settlement_outcome = ?1, settlement_reason = ?2, updated_at = ?3
                 WHERE id = ?4 AND state != 'settled'",
                params![outcome, reason, now, command_id],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Consume a single-use node enroll token, returning its id when it verifies.
///
/// Marks the row used inside the same write transaction as the caller's host
/// insert, so a token cannot enroll two hosts even under concurrent hellos.
fn consume_enroll_token<F>(
    conn: &Connection,
    presented: &str,
    now: &str,
    verify: &F,
) -> Result<Option<String>, StoreError>
where
    F: Fn(&str, &str) -> bool,
{
    let Some(prefix) = crate::auth::token_prefix(presented) else {
        return Ok(None);
    };
    // Indexed by prefix, so one candidate and one Argon2 verify per attempt
    // rather than a scan of every live token (A4).
    let candidate = conn
        .query_row(
            "SELECT id, token_hash FROM enroll_tokens
             WHERE token_prefix = ?1 AND used = 0 AND expires_at > ?2",
            params![prefix, now],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((id, hash)) = candidate else {
        return Ok(None);
    };
    if !verify(presented, &hash) {
        return Ok(None);
    }
    let claimed = conn.execute(
        "UPDATE enroll_tokens SET used = 1, used_at = ?1 WHERE id = ?2 AND used = 0",
        params![now, id],
    )?;
    if claimed == 0 {
        return Ok(None);
    }
    Ok(Some(id))
}

/// True when this instance id has been deleted and must never come back.
fn is_deleted_instance(conn: &Connection, instance_id: &str) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM deleted_instances WHERE instance_id = ?1",
            params![instance_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(crate) fn ensure_column(
    conn: &Connection,
    table: &str,
    name: &str,
    decl: &str,
) -> Result<(), rusqlite::Error> {
    if !column_exists(conn, table, name)? {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {decl}"), [])?;
    }
    Ok(())
}

/// Whether `table` currently has a column named `name`.
pub(crate) fn column_exists(
    conn: &Connection,
    table: &str,
    name: &str,
) -> Result<bool, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|col| col == name);
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mint an enroll token whose plaintext is its own "hash", so tests can use
    /// a trivial `verify` of string equality (D-018).
    async fn enroll_token(store: &Store, label: &str) -> String {
        // Real tokens are 64 hex chars; the prefix index only applies to those.
        let plaintext = hex_token(label);
        let plaintext = plaintext.as_str();
        store
            .insert_enroll_token(
                plaintext.to_string(),
                crate::auth::token_prefix(plaintext).map(str::to_string),
                "dev_test".into(),
                "2099-01-01T00:00:00.000Z".into(),
            )
            .await
            .expect("insert enroll token");
        plaintext.to_string()
    }

    /// Deterministic, distinct 64-hex token derived from a readable label.
    fn hex_token(label: &str) -> String {
        let mut hex: String = label.bytes().map(|b| format!("{b:02x}")).collect();
        hex.truncate(64);
        format!("{hex:0<64}")
    }

    fn verify_eq(presented: &str, hash: &str) -> bool {
        presented == hash
    }

    #[tokio::test]
    async fn host_lost_obeys_offline_grace_and_reconnect_and_preserves_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let host = new_id("hst").unwrap();
        store
            .authenticate_host(
                HostAuthRequest {
                    presented: enroll_token(&store, "enroll-reclaim").await,
                    hello_host_id: Some(host.clone()),
                    label: Some("reclaim-test".into()),
                    node_version: None,
                },
                verify_eq,
                |_| Ok("test-hash".into()),
            )
            .await
            .unwrap();
        // ma-lineage r6 item 3(c): only a chapter that REACHED a live
        // lifecycle may become host-lost. Acknowledge the instance (ready) so
        // it is the sweep's target; unacked requested rows belong to
        // expire_stale_requested instead.
        let instance = seed_acknowledged_instance(&store, &host).await;
        assert_eq!(
            store.expire_lost_hosts(0).await.unwrap().0,
            0,
            "online hosts never expire"
        );
        store.mark_host_offline(host.clone()).await.unwrap();
        assert_eq!(
            store.expire_lost_hosts(600_000).await.unwrap().0,
            0,
            "ten minute grace"
        );
        store
            .apply_inventory(host.clone(), Default::default(), None)
            .await
            .unwrap();
        assert_eq!(
            store.expire_lost_hosts(0).await.unwrap().0,
            0,
            "reconnect clears offline timer"
        );
        store.mark_host_offline(host).await.unwrap();
        assert_eq!(store.expire_lost_hosts(0).await.unwrap().0, 1);
        assert_eq!(
            store.expire_lost_hosts(0).await.unwrap().0,
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
        let token = enroll_token(&store, "enroll-1").await;
        let outcome = store
            .authenticate_host(
                HostAuthRequest {
                    presented: token,
                    hello_host_id: Some(host_id.clone()),
                    label: Some("slow-node".into()),
                    node_version: Some("test".into()),
                },
                verify_eq,
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
                None,
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

    /// A create RPC whose accept reply was lost past the Hub deadline is
    /// `queued + unknown`; the timeout arm marks it `reconciling`, and the
    /// Node's mirrored journal — not a resent RPC — wins: the journaled
    /// `accepted` flips the row to `accepted + clear`. The command is never
    /// resent (protocol §2.5).
    #[tokio::test]
    async fn a_lost_accept_reconciling_row_is_converged_by_the_journal_not_a_resend() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = Store::open(dir.path()).expect("store");
        let host_id = new_id("hst").expect("host id");
        let token = enroll_token(&store, "enroll-reconcile").await;
        let _ = store
            .authenticate_host(
                HostAuthRequest {
                    presented: token,
                    hello_host_id: Some(host_id.clone()),
                    label: Some("slow-node".into()),
                    node_version: Some("test".into()),
                },
                verify_eq,
                |_| Ok("test-hash".into()),
            )
            .await
            .expect("host enroll");
        let instance = store
            .insert_instance(
                host_id.clone(),
                None,
                "claude".into(),
                "shell-pty".into(),
                None,
                json!({}),
            )
            .await
            .expect("instance");
        let (command, _) = store
            .queue_command(
                None,
                Some(instance.instance_id.clone()),
                host_id.clone(),
                "instance.create".into(),
                json!({"instanceId": instance.instance_id}),
                None,
                None,
                None,
            )
            .await
            .expect("command");
        store
            .mark_forward_intent(command.command_id.clone())
            .await
            .expect("forward once");
        // The RPC accept deadline elapsed: unknown, not resent.
        let unknown = store
            .get_command(command.command_id.clone())
            .await
            .expect("query")
            .expect("row");
        assert_eq!(unknown.state, "queued");
        assert_eq!(unknown.resolution, "unknown");

        let reconciling = store
            .mark_reconciling(command.command_id.clone())
            .await
            .expect("reconciling");
        assert_eq!(reconciling.state, "queued");
        assert_eq!(reconciling.resolution, "reconciling");

        // The Node journaled its durable accept after the reply was lost.
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
                        "state": "accepted",
                        "entity": {
                            "commandId": command.command_id,
                            "state": "accepted"
                        }
                    }
                }),
            )
            .await
            .expect("journaled accept");
        let converged = store
            .get_command(command.command_id.clone())
            .await
            .expect("query")
            .expect("row");
        assert_eq!(converged.state, "accepted");
        assert_eq!(converged.resolution, "clear");
    }

    /// A forwarded send whose RPC reply is lost never leaves the three-state
    /// model: it rests at `queued` with resolution `unknown` / `reconciling`
    /// (§2.5, §12.2 — no fourth state, no speculative failure), and the Node's
    /// mirrored journal is what converges it to accepted and then settled.
    #[tokio::test]
    async fn a_never_acked_send_rests_unknown_until_the_journal_settles_it() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = Store::open(dir.path()).expect("store");
        let host_id = new_id("hst").expect("host id");
        let token = enroll_token(&store, "enroll-converge").await;
        let _ = store
            .authenticate_host(
                HostAuthRequest {
                    presented: token,
                    hello_host_id: Some(host_id.clone()),
                    label: Some("slow-node".into()),
                    node_version: Some("test".into()),
                },
                verify_eq,
                |_| Ok("test-hash".into()),
            )
            .await
            .expect("host enroll");
        let instance = store
            .insert_instance(
                host_id.clone(),
                None,
                "claude".into(),
                "shell-pty".into(),
                None,
                json!({}),
            )
            .await
            .expect("instance");
        let (command, _) = store
            .queue_command(
                None,
                Some(instance.instance_id.clone()),
                host_id.clone(),
                "instance.send".into(),
                json!({"instanceId": instance.instance_id}),
                None,
                None,
                None,
            )
            .await
            .expect("command");
        store
            .mark_forward_intent(command.command_id.clone())
            .await
            .expect("forward once");

        // The RPC accept timed out: the reply is lost, so the Hub delegates to
        // reconciliation. The row stays `queued` — never a fourth state — with
        // no settlement.
        let reconciling = store
            .mark_reconciling(command.command_id.clone())
            .await
            .expect("reconciling");
        assert_eq!(reconciling.state, "queued");
        assert_eq!(reconciling.resolution, "reconciling");
        assert!(reconciling.settlement.is_none());

        // The Node journaled its durable accept, then its completed settlement.
        for (state, outcome) in [
            ("accepted", Value::Null),
            (
                "settled",
                json!({
                    "state": "known",
                    "value": { "outcome": "completed", "resultRef": null, "error": null }
                }),
            ),
        ] {
            let mut entity = json!({ "commandId": command.command_id, "state": state });
            if outcome != Value::Null {
                entity["settlement"] = outcome;
            }
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
                            "entity": entity
                        }
                    }),
                )
                .await
                .expect("journaled lifecycle");
        }
        let settled = store
            .get_command(command.command_id.clone())
            .await
            .expect("query")
            .expect("row");
        assert_eq!(settled.state, "settled");
        assert_eq!(settled.resolution, "clear");
        let projection = settled.settlement.as_ref().expect("settlement projected");
        assert_eq!(projection.outcome, "completed");
        assert!(projection.reason.is_none());
        // The internal ledger columns never appear as wire fields.
        let wire = serde_json::to_value(&settled).expect("serialize");
        assert!(wire.get("settlementOutcome").is_none());
        assert!(wire.get("settlementReason").is_none());
        assert!(wire.get("reason").is_none());
    }

    /// An explicit Node rejection is the §2.5 `settled` + outcome `rejected`
    /// case — not a fourth state. A later positive settle frame must not
    /// regress the terminal rejection, and a settled row never carries a
    /// mismatched reason (the mark_settled hole from the previous round).
    #[tokio::test]
    async fn a_rejected_send_settles_rejected_and_a_later_settle_frame_keeps_it() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = Store::open(dir.path()).expect("store");
        let host_id = new_id("hst").expect("host id");
        let token = enroll_token(&store, "enroll-reject").await;
        let _ = store
            .authenticate_host(
                HostAuthRequest {
                    presented: token,
                    hello_host_id: Some(host_id.clone()),
                    label: Some("reject-node".into()),
                    node_version: Some("test".into()),
                },
                verify_eq,
                |_| Ok("test-hash".into()),
            )
            .await
            .expect("host enroll");
        let instance = store
            .insert_instance(
                host_id.clone(),
                None,
                "claude".into(),
                "shell-pty".into(),
                None,
                json!({}),
            )
            .await
            .expect("instance");
        let (command, _) = store
            .queue_command(
                None,
                Some(instance.instance_id.clone()),
                host_id.clone(),
                "instance.send".into(),
                json!({"instanceId": instance.instance_id}),
                None,
                None,
                None,
            )
            .await
            .expect("command");
        store
            .mark_forward_intent(command.command_id.clone())
            .await
            .expect("forward once");

        let rejected = store
            .reject_command(
                command.command_id.clone(),
                "instance.send requires input.text".into(),
            )
            .await
            .expect("reject")
            .expect("row rejected");
        assert_eq!(
            rejected.state, "settled",
            "a rejection settles, never a 4th state"
        );
        assert_eq!(rejected.resolution, "clear");
        let settlement = rejected.settlement.expect("settlement present");
        assert_eq!(settlement.outcome, "rejected");
        assert_eq!(
            settlement.reason.as_deref(),
            Some("instance.send requires input.text")
        );

        // The Node's positive settle frame (ws.rs mark_settled) must not
        // regress the terminal rejection nor leave a reason on a `completed`
        // row: the row stays rejected with its reason.
        let after_frame = store
            .mark_settled(command.command_id.clone(), host_id.clone())
            .await
            .expect("settle frame");
        assert_eq!(after_frame.state, "settled");
        assert_eq!(
            after_frame.settlement.as_ref().expect("settlement").outcome,
            "rejected",
            "terminal rejection is not overwritten by a later completed frame"
        );
        // A late accept cannot regress it either.
        let after_accept = store
            .mark_accepted(command.command_id.clone())
            .await
            .expect("accept");
        assert_eq!(after_accept.state, "settled");

        // A clean completion on a *different* command records completed with
        // no reason, proving mark_settled clears rather than preserves.
        let (other, _) = store
            .queue_command(
                None,
                Some(instance.instance_id.clone()),
                host_id.clone(),
                "instance.send".into(),
                json!({"instanceId": instance.instance_id}),
                None,
                None,
                None,
            )
            .await
            .expect("command");
        let completed = store
            .mark_settled(other.command_id.clone(), host_id.clone())
            .await
            .expect("settle");
        assert_eq!(completed.state, "settled");
        assert_eq!(
            completed.settlement.as_ref().expect("settlement").outcome,
            "completed"
        );
        assert!(
            completed
                .settlement
                .as_ref()
                .expect("settlement")
                .reason
                .is_none(),
            "a completed settlement never carries a reason"
        );
    }

    /// A database a round-2 build left behind carries `state='failed'` rows and
    /// a `reason` column. Reopening folds them into the §2.5 form — `settled`
    /// with a `rejected` settlement and the preserved reason — and drops the
    /// orphaned column (which also discards a stale reason on a settled row).
    #[tokio::test]
    async fn legacy_failed_rows_and_the_reason_column_are_migrated() {
        let dir = tempfile::tempdir().expect("data dir");
        let host_id = new_id("hst").expect("host id");
        let (failed_id, stale_id) = {
            let store = Store::open(dir.path()).expect("store");
            let token = enroll_token(&store, "enroll-legacy").await;
            let _ = store
                .authenticate_host(
                    HostAuthRequest {
                        presented: token,
                        hello_host_id: Some(host_id.clone()),
                        label: Some("legacy-node".into()),
                        node_version: Some("test".into()),
                    },
                    verify_eq,
                    |_| Ok("test-hash".into()),
                )
                .await
                .expect("host enroll");
            let instance = store
                .insert_instance(
                    host_id.clone(),
                    None,
                    "claude".into(),
                    "shell-pty".into(),
                    None,
                    json!({}),
                )
                .await
                .expect("instance");
            let mut ids = Vec::new();
            for _ in 0..2 {
                let (command, _) = store
                    .queue_command(
                        None,
                        Some(instance.instance_id.clone()),
                        host_id.clone(),
                        "instance.send".into(),
                        json!({"instanceId": instance.instance_id}),
                        None,
                        None,
                        None,
                    )
                    .await
                    .expect("command");
                ids.push(command.command_id);
            }
            (ids[0].clone(), ids[1].clone())
        };

        // Fabricate the round-2 on-disk shape behind the store's back: the old
        // `reason` column plus a `failed` row and a settled row with a stale
        // reason and no recorded settlement outcome.
        {
            let conn = Connection::open(dir.path().join("hub.sqlite")).expect("open db");
            conn.execute_batch("ALTER TABLE commands ADD COLUMN reason TEXT")
                .expect("legacy reason column");
            conn.execute(
                "UPDATE commands
                 SET state = 'failed', resolution = 'failed', reason = ?1
                 WHERE id = ?2",
                params!["node did not acknowledge", failed_id],
            )
            .expect("legacy failed row");
            conn.execute(
                "UPDATE commands
                 SET state = 'settled', resolution = 'clear', reason = ?1,
                     settlement_outcome = NULL, settlement_reason = NULL
                 WHERE id = ?2",
                params!["stale reason on a completed row", stale_id],
            )
            .expect("legacy settled row");
        }

        // Reopening runs the migration.
        let store = Store::open(dir.path()).expect("reopen migrates");
        let folded = store
            .get_command(failed_id.clone())
            .await
            .expect("query")
            .expect("row");
        assert_eq!(folded.state, "settled");
        assert_eq!(folded.resolution, "clear");
        let settlement = folded.settlement.as_ref().expect("rejected settlement");
        assert_eq!(settlement.outcome, "rejected");
        assert_eq!(
            settlement.reason.as_deref(),
            Some("node did not acknowledge")
        );

        let stale = store
            .get_command(stale_id.clone())
            .await
            .expect("query")
            .expect("row");
        assert_eq!(stale.state, "settled");
        assert!(
            stale.settlement.as_ref().is_none_or(|s| s.reason.is_none()),
            "the stale reason on a completed row is discarded: {:?}",
            stale.settlement
        );
        let wire = serde_json::to_value(&folded).expect("serialize");
        assert!(wire.get("reason").is_none());

        // The orphaned column is physically gone.
        let conn = Connection::open(dir.path().join("hub.sqlite")).expect("open db");
        let mut stmt = conn.prepare("PRAGMA table_info(commands)").expect("pragma");
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("cols")
            .filter_map(Result::ok)
            .collect();
        assert!(!columns.contains(&"reason".to_string()), "{columns:?}");
        assert!(columns.contains(&"settlement_outcome".to_string()));

        // Reopening again is a no-op (no `reason` column to migrate).
        Store::open(dir.path()).expect("idempotent reopen");
    }

    /// D-018: an enrolled host re-announces with its own stored node token,
    /// and that token authenticates its owner regardless of the hello hostId.
    #[tokio::test]
    async fn enrolled_host_reannounces_with_its_own_node_token() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = Store::open(dir.path()).expect("store");
        let host_id = new_id("hst").expect("host id");
        let token = enroll_token(&store, "enroll-reannounce").await;
        let first = store
            .authenticate_host(
                HostAuthRequest {
                    presented: token,
                    hello_host_id: Some(host_id.clone()),
                    label: Some("local-development".into()),
                    node_version: Some("1".into()),
                },
                verify_eq,
                |secret| Ok(secret.to_string()),
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

        let authenticated = store
            .authenticate_host(
                HostAuthRequest {
                    presented: node_token,
                    // A host token authenticates its owner, regardless of a
                    // different identity claimed in the hello payload.
                    hello_host_id: Some(new_id("hst").expect("other host")),
                    label: Some("attacker label".into()),
                    node_version: Some("2".into()),
                },
                verify_eq,
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

    /// D-018 + A1: an enroll token mints a new host and never claims an
    /// existing one, and it cannot be replayed for a second host.
    #[tokio::test]
    async fn enroll_token_is_single_use_and_cannot_claim_an_existing_host() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = Store::open(dir.path()).expect("store");
        let victim = new_id("hst").expect("host id");
        let token = enroll_token(&store, "enroll-once").await;
        let first = store
            .authenticate_host(
                HostAuthRequest {
                    presented: token.clone(),
                    hello_host_id: Some(victim.clone()),
                    label: None,
                    node_version: None,
                },
                verify_eq,
                |secret| Ok(secret.to_string()),
            )
            .await
            .expect("first enroll");
        assert!(matches!(
            first,
            HostAuthOutcome::Authenticated {
                node_token: Some(_),
                ..
            }
        ));

        // Same token again: consumed, so rejected even for a brand-new host id.
        let replay = store
            .authenticate_host(
                HostAuthRequest {
                    presented: token,
                    hello_host_id: Some(new_id("hst").expect("host id")),
                    label: None,
                    node_version: None,
                },
                verify_eq,
                |secret| Ok(secret.to_string()),
            )
            .await
            .expect("replay");
        assert!(
            matches!(replay, HostAuthOutcome::Rejected),
            "an enroll token must be single use"
        );

        // A *fresh* enroll token still cannot take over the existing host.
        let second = enroll_token(&store, "enroll-takeover").await;
        let takeover = store
            .authenticate_host(
                HostAuthRequest {
                    presented: second,
                    hello_host_id: Some(victim.clone()),
                    label: None,
                    node_version: None,
                },
                verify_eq,
                |secret| Ok(secret.to_string()),
            )
            .await
            .expect("takeover");
        assert!(
            matches!(takeover, HostAuthOutcome::Rejected),
            "enroll token must not authenticate an existing host (A1)"
        );
        let listed = store.list_hosts().await.expect("list");
        assert_eq!(listed.len(), 1, "{listed:?}");
    }

    /// An expired enroll token is rejected.
    #[tokio::test]
    async fn expired_enroll_token_is_rejected() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = Store::open(dir.path()).expect("store");
        let stale = hex_token("stale");
        store
            .insert_enroll_token(
                stale.clone(),
                crate::auth::token_prefix(&stale).map(str::to_string),
                "dev_test".into(),
                "2000-01-01T00:00:00.000Z".into(),
            )
            .await
            .expect("insert");
        let outcome = store
            .authenticate_host(
                HostAuthRequest {
                    presented: stale.clone(),
                    hello_host_id: None,
                    label: None,
                    node_version: None,
                },
                verify_eq,
                |secret| Ok(secret.to_string()),
            )
            .await
            .expect("expired");
        assert!(matches!(outcome, HostAuthOutcome::Rejected));
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
                    presented: enroll_token(&store, "enroll-configure").await,
                    hello_host_id: Some(host.clone()),
                    label: Some("configure-test".into()),
                    node_version: None,
                },
                verify_eq,
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
                None,
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
        // D-028 §9.1: the legacy `ultracode` tier normalizes to the level it
        // is equivalent to plus the flag. It is not a sixth level, so it is
        // never stored as one.
        assert_eq!(patched.effort_name.as_deref(), Some("xhigh"));
        assert_eq!(patched.effort_ultracode, Some(true));
        let reloaded = store
            .get_instance(instance.instance_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.model.as_deref(), Some("opus"));
        assert_eq!(reloaded.effort_name.as_deref(), Some("xhigh"));
        assert_eq!(reloaded.effort_ultracode, Some(true));
        let listed = store.list_instances(None).await.unwrap();
        assert_eq!(listed[0].effort_name.as_deref(), Some("xhigh"));
        assert_eq!(listed[0].effort_ultracode, Some(true));
    }

    /// Open a store with one authenticated host, for tests that only need a
    /// place to hang instances.
    async fn store_with_host(tag: &str) -> (tempfile::TempDir, Store, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let host = new_id("hst").unwrap();
        store
            .authenticate_host(
                HostAuthRequest {
                    presented: enroll_token(&store, tag).await,
                    hello_host_id: Some(host.clone()),
                    label: Some(tag.into()),
                    node_version: None,
                },
                verify_eq,
                |_| Ok("test-hash".into()),
            )
            .await
            .unwrap();
        (dir, store, host)
    }

    /// D-028 §9.1: every legacy tier name maps to a level by NAME, and a row
    /// written before D-028 reads back as the level it meant — including the
    /// ones whose index would have pointed somewhere else.
    #[tokio::test]
    async fn legacy_effort_tier_names_normalize_by_name_not_index() {
        let (_dir, store, host) = store_with_host("enroll-legacy-effort").await;
        // Claude rows use the Claude legacy table; unknown → high.
        for (legacy, index, level, ultracode) in [
            ("default", 0, "low", false),
            ("think", 1, "high", false),
            ("think-hard", 2, "xhigh", false),
            ("ultracode", 3, "xhigh", true),
            // Codex's index 3 was `ultra`, not `ultracode` — normalizing by
            // index instead of name would silently turn it into ultracode.
            ("max", 4, "max", false),
        ] {
            let instance = store
                .insert_instance(
                    host.clone(),
                    None,
                    "claude".into(),
                    "claude-print".into(),
                    Some(format!("legacy-{legacy}")),
                    json!({ "effortName": legacy, "effortIndex": index }),
                )
                .await
                .unwrap();
            let reloaded = store
                .get_instance(instance.instance_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reloaded.effort_name.as_deref(), Some(level), "{legacy}");
            assert_eq!(reloaded.effort_ultracode, Some(ultracode), "{legacy}");
            // The legacy index survives for older clients but never decided
            // the tier above.
            assert_eq!(reloaded.effort_index, Some(index), "{legacy}");
        }
        // Cross-crate: for EVERY legacy word the web/driver tables migrate,
        // the Hub store must land on exactly what the shared
        // remuda-protocol normalizer returns. This pins the three layers
        // (protocol table, Hub row, driver argv) to the one function.
        let cases = [
            ("claude", "default"),
            ("claude", "think"),
            ("claude", "think-hard"),
            ("claude", "ultra"),
            ("claude", "ultracode"),
            ("claude", "whatever"),
            ("codex", "minimal"),
            ("codex", "xhigh"),
            ("codex", "max"),
            ("codex", "ultra"),
            ("codex", "bogus"),
            ("grok", "quick"),
            ("grok", "standard"),
            ("grok", "max"),
            ("grok", "bogus"),
        ];
        for (kind, legacy) in cases {
            let expected = remuda_protocol::normalize_legacy_effort(
                remuda_protocol::effort_kind_from_str(kind),
                legacy,
            );
            // Insert below the maxInstances gate so the large cross-crate
            // table exercises load_instance's normalization directly.
            let instance_id = new_id("ins").unwrap();
            let journal_id = new_id("obj").unwrap();
            let now = now_rfc3339();
            let spec = json!({ "effortName": legacy, "effortIndex": 9 }).to_string();
            let reload_id = instance_id.clone();
            store
                .run_named("legacy_effort_tier_names_normalize_by_name_not_index", {
                    let host = host.clone();
                    move |conn| {
                        conn.execute(
                            "INSERT INTO instances
                                (id, host_id, workspace_id, kind, driver, lifecycle, activity,
                                 connectivity, title, journal_id, durable_seq, spec_json,
                                 created_at, updated_at)
                             VALUES (?1, ?2, NULL, ?3, 'generic-pty', 'running', 'idle',
                                     'connected', ?4, ?5, 0, ?6, ?7, ?7)",
                            params![
                                instance_id.clone(),
                                host,
                                kind,
                                format!("legacy-{kind}-{legacy}"),
                                journal_id,
                                spec,
                                now
                            ],
                        )?;
                        Ok(())
                    }
                })
                .await
                .unwrap();
            let reloaded = store.get_instance(reload_id).await.unwrap().unwrap();
            assert_eq!(
                reloaded.effort_name.as_deref(),
                Some(expected.level_name()),
                "{kind}:{legacy} disagrees with remuda-protocol"
            );
            assert_eq!(
                reloaded.effort_ultracode,
                Some(expected.ultracode),
                "{kind}:{legacy} disagrees with remuda-protocol"
            );
        }
    }

    #[tokio::test]
    async fn codex_top_efforts_round_trip_through_configure_and_reload() {
        let (_dir, store, host) = store_with_host("enroll-codex-top-efforts").await;
        let instance = store
            .insert_instance(
                host.clone(),
                None,
                "codex".into(),
                "generic-pty".into(),
                Some("codex-top-efforts".into()),
                json!({ "effortName": "minimal" }),
            )
            .await
            .unwrap();
        assert_eq!(instance.effort_name.as_deref(), Some("low"));

        for (index, name) in [(4, "max"), (5, "ultra"), (4, "max")] {
            let payload = json!({
                "instanceId": instance.instance_id,
                "effort": { "index": index, "name": name, "kind": "codex", "ultracode": false }
            });
            let (command, created) = store
                .queue_command(
                    None,
                    Some(instance.instance_id.clone()),
                    host.clone(),
                    "instance.configure".into(),
                    payload.clone(),
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            assert!(created);
            assert_eq!(command.payload["effort"]["name"], json!(name));
            assert_eq!(command.payload["effort"]["index"], json!(index));
            let patched = store
                .patch_instance_configure(instance.instance_id.clone(), payload)
                .await
                .unwrap();
            assert_eq!(patched.effort_name.as_deref(), Some(name));
            assert_eq!(patched.effort_ultracode, Some(false));
            let reloaded = store
                .get_instance(instance.instance_id.clone())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reloaded.effort_name.as_deref(), Some(name));
            assert_eq!(reloaded.effort_ultracode, Some(false));
            let listed = store.list_instances(None).await.unwrap();
            assert_eq!(listed[0].effort_name.as_deref(), Some(name));
        }
    }

    /// D-028 §1.0 rule 4: `launchedBy` is populated for rows that predate it.
    #[tokio::test]
    async fn launched_by_is_derived_from_mode_for_legacy_rows() {
        let (_dir, store, host) = store_with_host("enroll-launched-by").await;
        let instance = store
            .insert_instance(
                host.clone(),
                None,
                "terminal".into(),
                "shell-pty".into(),
                Some("legacy".into()),
                json!({}),
            )
            .await
            .unwrap();
        // No `mode` stored at all: Remuda's own launch path is what creates
        // rows, so that is what an absent mode means.
        let reloaded = store
            .get_instance(instance.instance_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.launched_by.as_deref(), Some("remuda"));
        // Promotion means an agent CLI took over a login shell's foreground,
        // which only happens because a person typed the command.
        let promoted_id = instance.instance_id.clone();
        store
            .run_named(
                "launched_by_is_derived_from_mode_for_legacy_rows",
                move |conn| {
                    conn.execute(
                        "UPDATE instances SET kind = ?1, mode = ?2, promoted_at = ?3 WHERE id = ?4",
                        params!["claude", "promoted", now_rfc3339(), promoted_id],
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        let promoted = store
            .get_instance(instance.instance_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(promoted.launched_by.as_deref(), Some("user"));
    }

    /// D-028 §1.0 rule 4, the case the legacy derivation gets wrong.
    #[tokio::test]
    async fn a_remuda_launched_agent_stays_remuda_after_it_promotes() {
        // §1.0 rule 2 makes promotion the only detection path, so a
        // Remuda-launched agent promotes too and `mode == promoted` stopped
        // meaning "a human typed it". Measured on a live Node before this fix:
        // both launch paths read back as `user`. What separates them is what
        // the row was *before* — `terminal` is a shell someone typed into.
        let (_dir, store, host) = store_with_host("enroll-launched-by-native").await;
        for (created_as, expected) in [("claude", "remuda"), ("terminal", "user")] {
            let instance = store
                .insert_instance(
                    host.clone(),
                    None,
                    created_as.into(),
                    "shell-pty".into(),
                    Some(format!("{created_as}-session")),
                    json!({}),
                )
                .await
                .unwrap();
            let id = instance.instance_id.clone();
            store
                .append_journal(
                    host.clone(),
                    id.clone(),
                    None,
                    json!({
                        "kind": "lifecycle",
                        "payload": {
                            "type": "native",
                            "nativeName": "agent_promoted",
                            "relatedIds": {"kind": "claude", "mode": "promoted",
                                           "promotedAt": now_rfc3339()},
                        },
                    }),
                )
                .await
                .unwrap();
            let promoted = store.get_instance(id.clone()).await.unwrap().unwrap();
            assert_eq!(
                promoted.launched_by.as_deref(),
                Some(expected),
                "a session created as {created_as} that promotes to claude"
            );

            // A demote/repromote cycle must not rewrite where it came from.
            store
                .append_journal(
                    host.clone(),
                    id.clone(),
                    None,
                    json!({
                        "kind": "lifecycle",
                        "payload": {
                            "type": "native",
                            "nativeName": "agent_promoted",
                            "relatedIds": {"kind": "claude", "mode": "promoted",
                                           "promotedAt": now_rfc3339()},
                        },
                    }),
                )
                .await
                .unwrap();
            let again = store.get_instance(id).await.unwrap().unwrap();
            assert_eq!(again.launched_by.as_deref(), Some(expected));
        }
    }

    /// Backdate a row so time-window behaviour is testable without sleeping.
    async fn backdate_instance(store: &Store, instance_id: &str, minutes: i64) {
        let instance_id = instance_id.to_owned();
        store
            .run_named("backdate_instance", move |conn| {
                let then = time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes);
                let stamp = format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
                    then.year(),
                    u8::from(then.month()),
                    then.day(),
                    then.hour(),
                    then.minute(),
                    then.second()
                );
                conn.execute(
                    "UPDATE instances SET created_at = ?1, updated_at = ?1 WHERE id = ?2",
                    params![stamp, instance_id],
                )?;
                Ok(())
            })
            .await
            .expect("backdate");
    }

    async fn seed_instance(store: &Store, host_id: &str) -> InstanceRecord {
        store
            .insert_instance(
                host_id.to_owned(),
                None,
                "terminal".into(),
                "shell-pty".into(),
                None,
                json!({}),
            )
            .await
            .expect("insert instance")
    }

    /// Seed a row the Node has acknowledged, which is what a restart reconcile
    /// is about: `insert_instance` alone leaves it `requested` — an intent no
    /// Node has answered, which is deliberately outside that reconcile's scope.
    async fn seed_acknowledged_instance(store: &Store, host_id: &str) -> InstanceRecord {
        let instance = seed_instance(store, host_id).await;
        store
            .append_journal(
                host_id.to_owned(),
                instance.instance_id.clone(),
                None,
                json!({
                    "kind": "lifecycle",
                    "payload": {
                        "type": "entity", "entityType": "instance", "state": "ready"
                    }
                }),
            )
            .await
            .expect("acknowledge");
        store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row")
    }

    /// c-cardsettle: journal a `pending` approval interaction for an instance
    /// (the durable row the inbox/badge read). Returns the interaction id.
    async fn seed_pending_interaction(store: &Store, host_id: &str, instance_id: &str) -> String {
        let id = new_id("int").expect("interaction id");
        store
            .append_journal(
                host_id.to_owned(),
                instance_id.to_owned(),
                None,
                json!({
                    "kind": "interaction.requested",
                    "payload": {
                        "interactionKind": "approval",
                        "interaction": {
                            "id": id,
                            "kind": "approval",
                            "state": "pending",
                            "blocking": true,
                            "answerable": true,
                            "carrier": "harness-hook",
                            "deadline": { "state": "unknown" },
                            "resolution": { "state": "unknown" },
                            "request": {
                                "kind": "approval",
                                "title": "Bash",
                                "description": "echo e2e",
                                "options": [],
                            }
                        }
                    }
                }),
            )
            .await
            .expect("journal interaction.requested");
        let rows = store
            .list_interactions(None, Some(instance_id.to_owned()), None, false)
            .await
            .expect("list");
        assert!(rows.iter().any(|r| r.interaction_id == id));
        id
    }

    /// Read the durable state + resolution reason of one interaction.
    async fn interaction_state_and_reason(
        store: &Store,
        interaction_id: &str,
    ) -> (String, Option<String>) {
        let row = store
            .get_interaction(interaction_id.to_owned())
            .await
            .expect("get")
            .expect("interaction row");
        let reason = row
            .payload
            .pointer("/payload/interaction/resolution/value/reason")
            .or_else(|| {
                row.payload
                    .pointer("/payload/entity/resolution/value/reason")
            })
            .and_then(Value::as_str)
            .map(str::to_owned);
        (row.state, reason)
    }

    /// Age an interaction row's updated_at so the 24 h departed retention can
    /// be tested without sleeping.
    /// c-cardsettle r3 item 4: the replay-on-connect window returns recent
    /// invalidated rows (live and tombstoned) but not aged or pending rows.
    #[tokio::test]
    async fn recent_invalidated_interactions_replay_window() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "recent-invalidated").await;

        // Fresh invalidated (live row): included.
        let fresh = seed_acknowledged_instance(&store, &host).await;
        let fresh_int = seed_pending_interaction(&store, &host, &fresh.instance_id).await;
        let (_, _) = store
            .settle_instance_exited(fresh.instance_id.clone(), "x".into())
            .await
            .expect("settle");

        // Aged invalidated: excluded (older than the 5-minute replay window).
        let aged = seed_acknowledged_instance(&store, &host).await;
        let aged_int = seed_pending_interaction(&store, &host, &aged.instance_id).await;
        let (_, _) = store
            .settle_instance_exited(aged.instance_id.clone(), "x".into())
            .await
            .expect("settle");
        backdate_interaction(&store, &aged_int, 1).await;

        // Tombstoned fresh: included.
        let deleted = seed_acknowledged_instance(&store, &host).await;
        let deleted_int = seed_pending_interaction(&store, &host, &deleted.instance_id).await;
        let (_, _) = store
            .settle_instance_exited(deleted.instance_id.clone(), "x".into())
            .await
            .expect("settle");
        assert!(
            store
                .delete_instance(deleted.instance_id)
                .await
                .expect("delete")
        );

        // Still-pending: never replayed as a settlement.
        let pending = seed_acknowledged_instance(&store, &host).await;
        let _pending_int = seed_pending_interaction(&store, &host, &pending.instance_id).await;

        let recent = store
            .recent_invalidated_interactions()
            .await
            .expect("recent");
        let ids: std::collections::HashSet<String> =
            recent.into_iter().map(|(_, id, _reason)| id).collect();
        assert!(ids.contains(&fresh_int), "fresh invalidated replayed");
        assert!(ids.contains(&deleted_int), "fresh tombstone replayed");
        assert!(!ids.contains(&aged_int), "aged invalidated outside window");
        assert!(
            !ids.iter().any(|id| id == &_pending_int),
            "pending rows are not settlements"
        );
        store.close().await;
    }

    /// c-cardsettle r5 item 6: the lag recovery cursor replaces the fixed
    /// 5-minute window for a follower that MISSED notices. An older lost
    /// settlement (beyond the reconnect snapshot window) is still recovered on
    /// the first cursor page; pages move strictly forward from the cursor;
    /// tombstones page with live rows; pending rows never appear.
    #[tokio::test]
    async fn lag_cursor_recovers_an_older_lost_settlement_and_pages_forward() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "lag-cursor").await;

        // Fresh live invalidated row.
        let fresh = seed_acknowledged_instance(&store, &host).await;
        let fresh_int = seed_pending_interaction(&store, &host, &fresh.instance_id).await;
        let (_, _) = store
            .settle_instance_exited(fresh.instance_id.clone(), "x".into())
            .await
            .expect("settle");

        // OLD live invalidated row, 2 hours back: outside the reconnect
        // snapshot window but a lost settlement the lag cursor must recover.
        let aged = seed_acknowledged_instance(&store, &host).await;
        let aged_int = seed_pending_interaction(&store, &host, &aged.instance_id).await;
        let (_, _) = store
            .settle_instance_exited(aged.instance_id.clone(), "x".into())
            .await
            .expect("settle");
        backdate_interaction(&store, &aged_int, 2).await;
        let aged_ts = interaction_updated_at(&store, &aged_int).await;

        // OLD tombstone (deleted instance), also 2 hours back.
        let deleted = seed_acknowledged_instance(&store, &host).await;
        let deleted_int = seed_pending_interaction(&store, &host, &deleted.instance_id).await;
        let (_, _) = store
            .settle_instance_exited(deleted.instance_id.clone(), "x".into())
            .await
            .expect("settle");
        let deleted_id = deleted.instance_id.clone();
        assert!(
            store
                .delete_instance(deleted.instance_id)
                .await
                .expect("delete")
        );
        backdate_tombstone(&store, &deleted_int, 2).await;

        // A still-pending row is never a settlement.
        let pending = seed_acknowledged_instance(&store, &host).await;
        let pending_int = seed_pending_interaction(&store, &host, &pending.instance_id).await;

        // The reconnect snapshot window still excludes the old rows.
        let snapshot_ids: std::collections::HashSet<String> = store
            .recent_invalidated_interactions()
            .await
            .expect("snapshot")
            .into_iter()
            .map(|(_, id, _)| id)
            .collect();
        assert!(snapshot_ids.contains(&fresh_int));
        assert!(!snapshot_ids.contains(&aged_int));
        assert!(!snapshot_ids.contains(&deleted_int));

        // First lag page (no cursor): bounded authoritative page INCLUDING the
        // old lost settlement and the old tombstone, excluding the pending row.
        let first = store
            .invalidated_interactions_after(None)
            .await
            .expect("first page");
        let first_ids: std::collections::HashSet<String> =
            first.iter().map(|(_, id, _, _)| id.clone()).collect();
        assert!(first_ids.contains(&fresh_int), "fresh settlement recovered");
        assert!(
            first_ids.contains(&aged_int),
            "the older lost settlement is recovered even outside the window"
        );
        assert!(
            first_ids.contains(&deleted_int),
            "an old tombstone is recovered with live rows"
        );
        assert!(!first_ids.contains(&pending_int), "pending never recovered");
        // r6 item 2: lag pages always walk OLDEST-first, so a multi-page drain
        // reaches every missed row; the aged row (2 hours old) sorts before the
        // fresh one.
        assert!(
            first
                .iter()
                .map(|(_, _, _, ts)| ts)
                .is_sorted_by(|a, b| a <= b),
            "lag pages are oldest-first"
        );

        // Paging strictly forward from a cursor PAST every row yields nothing.
        let last = first.last().expect("last row");
        let beyond_token = Store::settlement_cursor_of(&last.3, &last.1);
        assert!(
            store
                .invalidated_interactions_after(Some(beyond_token))
                .await
                .expect("page beyond newest")
                .is_empty(),
            "no settlement is newer than the newest row"
        );
        // Paging from the OLD row's composite cursor returns everything newer
        // (oldest-first across cursor pages), but not the old row itself.
        let aged_cursor = Store::settlement_cursor_of(&aged_ts, &aged_int);
        let onward = store
            .invalidated_interactions_after(Some(aged_cursor))
            .await
            .expect("page after old cursor");
        let onward_ids: Vec<String> = onward.iter().map(|(_, id, _, _)| id.clone()).collect();
        assert!(onward_ids.contains(&fresh_int));
        assert!(!onward_ids.contains(&aged_int), "cursor is exclusive");
        assert!(
            onward
                .iter()
                .map(|(_, _, _, ts)| ts)
                .is_sorted_by(|a, b| a <= b),
            "cursor pages walk oldest-first"
        );
        assert!(onward.len() <= SETTLEMENT_LAG_PAGE as usize);

        // r5 item 6 tie edge: another invalidated row with the SAME
        // updated_at as the aged row but a different id is not skipped by a
        // cursor taken on the aged row.
        let tied = seed_acknowledged_instance(&store, &host).await;
        let tied_int = seed_pending_interaction(&store, &host, &tied.instance_id).await;
        let (_, _) = store
            .settle_instance_exited(tied.instance_id.clone(), "x".into())
            .await
            .expect("settle tied");
        store
            .run_named("backdate_to_aged", {
                let aged_ts = aged_ts.clone();
                let tied_int = tied_int.clone();
                move |conn| {
                    conn.execute(
                        "UPDATE interactions SET updated_at = ?1 WHERE id = ?2",
                        params![aged_ts, tied_int],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("backdate tied row to the same millisecond");
        let after_tie = store
            .invalidated_interactions_after(Some(Store::settlement_cursor_of(&aged_ts, &aged_int)))
            .await
            .expect("tie page");
        assert!(
            after_tie.iter().any(|(_, id, _, _)| id == &tied_int),
            "a same-millisecond row with a later id follows the cursor"
        );
        let _ = deleted_id;
        store.close().await;
    }

    /// c-cardsettle r5 item 6: the lag page is hard-bounded for live rows and
    /// tombstones together, even with a backlog larger than the page.
    #[tokio::test]
    async fn lag_cursor_page_is_bounded_for_rows_and_tombstones() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let total = SETTLEMENT_LAG_PAGE + 20;
        store
            .run_named("seed_invalidated_backlog", move |conn| {
                {
                    let mut stmt = conn.prepare(
                        "INSERT INTO interactions
                            (id, instance_id, host_id, kind, state, blocking,
                             payload_json, created_at, updated_at)
                         VALUES (?1, ?2, ?3, 'approval', 'invalidated', 0, '{}',
                                 ?4, ?4)",
                    )?;
                    for n in 0..total {
                        let stamp = format!("2026-09-01T00:{n:04}.000Z");
                        stmt.execute(params![
                            format!("int_backlog_{n:05}"),
                            "ins_backlog",
                            "hst_backlog",
                            stamp
                        ])?;
                    }
                }
                Ok(())
            })
            .await
            .expect("seed");
        let page = store
            .invalidated_interactions_after(None)
            .await
            .expect("bounded page");
        assert_eq!(
            page.len(),
            SETTLEMENT_LAG_PAGE as usize,
            "rows and tombstones are bounded together by one LIMIT"
        );
        // Oldest-first (r6 item 2): the first bounded page is the OLDEST 512;
        // the newest 20 wait for the second page, which must be short.
        let ids: std::collections::HashSet<String> =
            page.iter().map(|(_, id, _, _)| id.clone()).collect();
        assert!(ids.contains("int_backlog_00000"));
        assert!(ids.contains(&format!("int_backlog_{:05}", SETTLEMENT_LAG_PAGE - 1)));
        assert!(!ids.contains(&format!("int_backlog_{:05}", SETTLEMENT_LAG_PAGE)));
        assert!(!ids.contains(&format!("int_backlog_{:05}", total - 1)));
        // The page is ordered ascending end to end.
        assert!(
            page.iter()
                .map(|(_, _, _, ts)| ts)
                .is_sorted_by(|a, b| a <= b)
        );

        // Draining from the page's last cursor returns exactly the remaining
        // 20 newest rows, and one more page is empty: the follower drains the
        // whole backlog and never skips the tail.
        let last = page.last().expect("last row");
        let cursor = Store::settlement_cursor_of(&last.3, &last.1);
        let second = store
            .invalidated_interactions_after(Some(cursor))
            .await
            .expect("second page");
        assert_eq!(second.len(), (total - SETTLEMENT_LAG_PAGE) as usize);
        let second_ids: Vec<String> = second.iter().map(|(_, id, _, _)| id.clone()).collect();
        assert_eq!(
            second_ids.first().map(String::as_str),
            Some(format!("int_backlog_{:05}", SETTLEMENT_LAG_PAGE).as_str())
        );
        assert_eq!(
            second_ids.last().map(String::as_str),
            Some(format!("int_backlog_{:05}", total - 1).as_str())
        );
        let tail_cursor =
            Store::settlement_cursor_of(&second.last().unwrap().3, &second.last().unwrap().1);
        assert!(
            store
                .invalidated_interactions_after(Some(tail_cursor))
                .await
                .expect("third page")
                .is_empty()
        );
        store.close().await;
    }

    /// Read one interaction's durable updated_at (lag cursor test helper).
    async fn interaction_updated_at(store: &Store, interaction_id: &str) -> String {
        let interaction_id = interaction_id.to_owned();
        store
            .run_named("interaction_updated_at", move |conn| {
                conn.query_row(
                    "SELECT updated_at FROM interactions WHERE id = ?1",
                    params![interaction_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(StoreError::from)
            })
            .await
            .expect("updated_at")
            .expect("interaction row")
    }

    /// Backdate a delete-instance tombstone (lag cursor test helper).
    async fn backdate_tombstone(store: &Store, interaction_id: &str, hours: i64) {
        let interaction_id = interaction_id.to_owned();
        store
            .run_named("backdate_tombstone", move |conn| {
                let then = time::OffsetDateTime::now_utc() - time::Duration::hours(hours);
                let stamp = format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
                    then.year(),
                    u8::from(then.month()),
                    then.day(),
                    then.hour(),
                    then.minute(),
                    then.second()
                );
                conn.execute(
                    "UPDATE interaction_tombstones SET updated_at = ?1, created_at = ?1
                     WHERE id = ?2",
                    params![stamp, interaction_id],
                )?;
                Ok(())
            })
            .await
            .expect("backdate tombstone");
    }

    async fn backdate_interaction(store: &Store, interaction_id: &str, hours: i64) {
        let interaction_id = interaction_id.to_owned();
        store
            .run_named("backdate_interaction", move |conn| {
                let then = time::OffsetDateTime::now_utc() - time::Duration::hours(hours);
                let stamp = format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
                    then.year(),
                    u8::from(then.month()),
                    then.day(),
                    then.hour(),
                    then.minute(),
                    then.second()
                );
                conn.execute(
                    "UPDATE interactions SET updated_at = ?1 WHERE id = ?2",
                    params![stamp, interaction_id],
                )?;
                Ok(())
            })
            .await
            .expect("backdate interaction");
    }

    /// D-047: the instance projection carries the *observed* route, so it must
    /// not be derived from the spec's *requested* route.
    ///
    /// The two types differ exactly where it matters: a request may say
    /// `route: "auto"` (let the Hub decide), while an observation only ever
    /// names a resolved route. Copying the spec's value into this field would
    /// therefore either drop it (auto is not an `ApiRouteKind`) or, worse,
    /// report what was asked for as what ran — the D-035 failure this field
    /// exists to prevent. A spec that does carry a route must still read back
    /// as "not proxied" until the Node's create result populates it.
    #[tokio::test]
    async fn instance_projection_never_infers_api_route_from_the_spec() {
        let (_dir, store, host) = store_with_host("api-route-projection").await;
        let routed = store
            .insert_instance(
                host.clone(),
                None,
                "claude".into(),
                "shell-pty".into(),
                None,
                json!({
                    "delegation": "gateway",
                    "apiRoute": {
                        "mode": "via",
                        "viaHostId": "hst_01993ab0-0000-7000-8000-000000000007",
                        "route": "hub-relay"
                    }
                }),
            )
            .await
            .expect("insert instance");
        let read = store
            .get_instance(routed.instance_id.clone())
            .await
            .expect("read")
            .expect("present");
        assert_eq!(
            read.api_route, None,
            "the projection is an observation: it is set from the Node's create \
             result, never copied off the requested route"
        );

        // And it stays absent on the wire, so an existing instance's JSON is
        // byte-identical to what a pre-D-047 Hub emitted.
        let wire = serde_json::to_value(&read).expect("serialize");
        assert!(
            wire.get("apiRoute").is_none(),
            "an unproxied instance must not gain an apiRoute key"
        );

        // And once the Node echoes the observed route, the projection column
        // carries it — still without touching the requested spec route.
        let echoed = remuda_protocol::ApiRoute::via(
            "hst_01993ab0-0000-7000-8000-000000000007"
                .parse()
                .expect("host id"),
            Some("proxy-label".into()),
            remuda_protocol::ApiRouteKind::HubRelay,
        );
        store
            .reconcile_instance_api_route(routed.instance_id.clone(), &echoed)
            .await
            .expect("project route");
        let observed = store
            .get_instance(routed.instance_id.clone())
            .await
            .expect("read")
            .expect("present");
        assert_eq!(observed.api_route.as_ref(), Some(&echoed));
        let wire = serde_json::to_value(&observed).expect("serialize");
        assert_eq!(wire["apiRoute"]["mode"], json!("via"));
        assert_eq!(wire["apiRoute"]["route"], json!("hub-relay"));
    }

    #[tokio::test]
    async fn promoted_hook_activity_is_mirrored_without_screen_or_subagent_override() {
        let (_dir, store, host) = store_with_host("hook-activity").await;
        let instance = seed_instance(&store, &host).await;
        let id = instance.instance_id;
        let authoritative = |activity: &str| {
            json!({"kind":"lifecycle","payload":{
                "type":"entity","entityType":"instance","state":"ready","entity":{
                    "nativeRef":{"signalTier":"hook"}, "activity":{"state":"known","value":activity}
                }
            }})
        };
        let native = |name: &str, channel: &str, activity: &str| {
            json!({
                "kind":"lifecycle","source":{"driverKind":"shell-pty","channel":channel},
                "payload":{"type":"native","nativeName":name,"status":{"state":"known","value":activity}}
            })
        };
        let validated = |name: &str, channel: &str, activity: &str| {
            let mut event = native(name, channel, activity);
            event["payload"]["relatedIds"] = json!({"remudaActivity":activity});
            event
        };
        for (seq, event, expected) in [
            (1, authoritative("idle"), "idle"),
            (2, native("UserPromptSubmit", "hook", "working"), "idle"),
            (
                3,
                validated("UserPromptSubmit", "hook", "working"),
                "working",
            ),
            (4, native("agent_status", "pty", "idle"), "working"),
            (5, native("Stop", "hook", "idle"), "working"),
            (6, authoritative("working"), "working"),
            (7, validated("interrupted", "pty", "idle"), "idle"),
            (8, authoritative("idle"), "idle"),
            (9, validated("SubagentStop", "hook", "working"), "idle"),
        ] {
            store
                .append_journal(host.clone(), id.clone(), Some(seq), event)
                .await
                .unwrap();
            let current = store.get_instance(id.clone()).await.unwrap().unwrap();
            assert_eq!(current.activity, expected, "seq {seq}");
            assert_eq!(current.signal_tier.as_deref(), Some("hook"));
        }
        store
            .append_journal(
                host,
                id.clone(),
                Some(10),
                json!({"kind":"lifecycle","payload":{
                    "type":"native","nativeName":"agent_demoted","relatedIds":{"kind":"terminal"}
                }}),
            )
            .await
            .unwrap();
        assert_eq!(
            store.get_instance(id).await.unwrap().unwrap().signal_tier,
            None
        );
        store.close().await;
    }

    /// The demo wedge: stale `requested` rows must stop holding placement slots,
    /// and the sweeper must eventually fail them outright.
    #[tokio::test]
    async fn stale_requested_instances_free_their_placement_slot_and_expire() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cap-node").await;

        let fresh = seed_instance(&store, &host).await;
        let stale = seed_instance(&store, &host).await;
        backdate_instance(&store, &stale.instance_id, 60).await;
        assert_eq!(
            store.running_count(host.clone()).await.expect("count"),
            0,
            "placement counts only Node-confirmed instances"
        );

        let (expired, _settlement) = store
            .expire_stale_requested(REQUESTED_SLOT_WINDOW_MS)
            .await
            .expect("sweep");
        assert_eq!(
            expired,
            vec![(host.clone(), stale.instance_id.clone())],
            "only the aged row expires"
        );
        let stale = store
            .get_instance(stale.instance_id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(stale.lifecycle, "failed");
        assert_eq!(
            stale.last_error.as_deref(),
            Some("create-never-acknowledged")
        );
        let fresh = store
            .get_instance(fresh.instance_id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(fresh.lifecycle, "requested", "a fresh create is untouched");
        assert_eq!(store.running_count(host).await.expect("count"), 0);
        store.close().await;
    }

    /// `ready`/`running`/`blocked` rows keep counting; `exited`/`failed` never do.
    #[tokio::test]
    async fn placement_slots_count_node_confirmed_lifecycles_only() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cap-node").await;

        let running = seed_instance(&store, &host).await;
        store
            .append_journal(
                host.clone(),
                running.instance_id.clone(),
                Some(1),
                json!({"kind":"lifecycle","payload":{"type":"entity","entityType":"instance","state":"ready"}}),
            )
            .await
            .expect("ready");
        // A blocked instance is still occupying its slot.
        store
            .append_journal(
                host.clone(),
                running.instance_id.clone(),
                Some(2),
                json!({"kind":"lifecycle","payload":{
                    "type":"native","nativeName":"agent_status",
                    "status":{"state":"known","value":"blocked"}}}),
            )
            .await
            .expect("blocked");
        let blocked = store
            .get_instance(running.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(blocked.lifecycle, "running");
        assert_eq!(blocked.activity, "blocked");
        assert_eq!(store.running_count(host.clone()).await.expect("count"), 1);

        let gone = seed_instance(&store, &host).await;
        store
            .append_journal(
                host.clone(),
                gone.instance_id.clone(),
                Some(1),
                json!({"kind":"lifecycle","payload":{"type":"entity","entityType":"instance","state":"exited"}}),
            )
            .await
            .expect("exited");
        assert_eq!(
            store.running_count(host.clone()).await.expect("count"),
            1,
            "an exited instance releases its slot"
        );

        // The insert guard still fences a burst: fresh `requested` rows count
        // there, so a host at its ceiling refuses another create…
        store
            .patch_host(
                host.clone(),
                None,
                None,
                Some(2),
                None,
                Default::default(),
                None,
            )
            .await
            .expect("cap 2");
        let pending = seed_instance(&store, &host).await;
        let refused = store
            .insert_instance(
                host.clone(),
                None,
                "terminal".into(),
                "shell-pty".into(),
                None,
                json!({}),
            )
            .await;
        assert!(
            matches!(refused, Err(StoreError::Id(ref message)) if message.contains("maxInstances")),
            "{refused:?}"
        );
        // …but an aged one no longer blocks anything.
        backdate_instance(&store, &pending.instance_id, 60).await;
        store
            .insert_instance(
                host,
                None,
                "terminal".into(),
                "shell-pty".into(),
                None,
                json!({}),
            )
            .await
            .expect("a stale requested row must not hold the last slot");
        store.close().await;
    }

    /// Node restart: rows the Node no longer lists exit, the rest are kept.
    #[tokio::test]
    async fn node_epoch_change_reconciles_only_unreported_instances() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "epoch-node").await;

        assert!(
            !store
                .record_node_epoch(host.clone(), Some("epoch_a".into()))
                .await
                .expect("first epoch"),
            "the first epoch a host announces is not a restart"
        );
        assert!(
            !store
                .record_node_epoch(host.clone(), Some("epoch_a".into()))
                .await
                .expect("same epoch"),
            "a reconnect on the same epoch is not a restart"
        );
        assert!(
            store
                .record_node_epoch(host.clone(), Some("epoch_b".into()))
                .await
                .expect("new epoch"),
            "a different epoch is a Node restart"
        );

        let kept = seed_acknowledged_instance(&store, &host).await;
        let lost = seed_acknowledged_instance(&store, &host).await;
        // A create still in flight: the Node has not answered it, so a restart
        // in that window says nothing about whether it was lost. Its own
        // age-bounded reaper is what eventually settles it.
        let in_flight = seed_instance(&store, &host).await;
        let (reconciled, _settlement) = store
            .reconcile_reported_instances(
                host.clone(),
                vec![kept.instance_id.clone()],
                "node-epoch-changed".into(),
                true,
            )
            .await
            .expect("reconcile");
        assert_eq!(
            reconciled,
            vec![lost.instance_id.clone()],
            "only the acknowledged row the node no longer lists is lost"
        );
        let lost = store
            .get_instance(lost.instance_id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(lost.lifecycle, "exited");
        assert_eq!(lost.last_error.as_deref(), Some("node-epoch-changed"));
        let kept = store
            .get_instance(kept.instance_id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            kept.lifecycle, "running",
            "an acknowledged row the node still lists survives unchanged"
        );
        let in_flight = store
            .get_instance(in_flight.instance_id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            in_flight.lifecycle, "requested",
            "a create in flight is not this reconcile's to settle"
        );
        store.close().await;
    }

    /// ma-lineage r6 item 3(a): on a nodeEpoch change an evidence-less
    /// host-lost/ambiguous terminal row the new Node does not report is
    /// reconciled to exited WITH ended_at (process-end evidence), releasing
    /// its seat/fan-out forever. A non-epoch reconcile leaves it potentially
    /// live (no ended_at).
    #[tokio::test]
    async fn epoch_change_stamps_end_evidence_on_unreported_host_lost_rows() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "epoch-hostlost").await;

        let host_lost = seed_acknowledged_instance(&store, &host).await;
        let ambiguous = seed_acknowledged_instance(&store, &host).await;
        let db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        db.execute(
            "UPDATE instances
                SET lifecycle='exited', ended_at=NULL, last_error='host-lost'
              WHERE id=?1",
            rusqlite::params![host_lost.instance_id],
        )
        .unwrap();
        db.execute(
            "UPDATE instances
                SET lifecycle='failed', ended_at=NULL, last_error='api-error: 429'
              WHERE id=?1",
            rusqlite::params![ambiguous.instance_id],
        )
        .unwrap();
        drop(db);

        // Same-epoch (non-epoch) reconcile must NOT stamp end evidence.
        let (_, _) = store
            .reconcile_reported_instances(host.clone(), vec![], "reconnect".into(), false)
            .await
            .unwrap();
        let db_path = dir.path().join("hub.sqlite");
        let read_ended = |path: &std::path::Path, id: &str| -> Option<String> {
            let db = rusqlite::Connection::open(path).unwrap();
            db.query_row(
                "SELECT ended_at FROM instances WHERE id=?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(
            read_ended(&db_path, &host_lost.instance_id).is_none(),
            "non-epoch reconcile leaves host-lost row without end evidence"
        );

        // Epoch change + not reported: stamp ended_at on both.
        store
            .record_node_epoch(host.clone(), Some("a".into()))
            .await
            .unwrap();
        store
            .record_node_epoch(host.clone(), Some("b".into()))
            .await
            .unwrap();
        let (lost, _) = store
            .reconcile_reported_instances(host.clone(), vec![], "node-epoch-changed".into(), true)
            .await
            .unwrap();
        assert_eq!(lost.len(), 2, "both evidence-less terminal rows reconciled");
        for id in [&host_lost.instance_id, &ambiguous.instance_id] {
            assert!(
                read_ended(&db_path, id).is_some(),
                "epoch change stamps end evidence for {id}"
            );
        }
        store.close().await;
    }

    /// ma-lineage r6 item 3(c): the host-lost sweep only touches chapters that
    /// reached a live lifecycle — a `requested` row is left to the stale-create
    /// reaper with its attested marker intact, and an attested failed row is
    /// left alone.
    #[tokio::test]
    async fn host_lost_sweep_skips_requested_and_attested_failed_rows() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "hostlost-skip").await;
        store
            .run_named("hostlost-setoffline", {
                let host = host.clone();
                move |conn| {
                    conn.execute(
                        "UPDATE hosts SET state='unreachable', offline_since='2000-01-01T00:00:00Z'
                         WHERE id=?1",
                        [host.as_str()],
                    )?;
                    Ok(())
                }
            })
            .await
            .unwrap();

        let requested = seed_instance(&store, &host).await;
        let attested = seed_acknowledged_instance(&store, &host).await;
        let db_path = dir.path().join("hub.sqlite");
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute(
            "UPDATE instances SET lifecycle='failed', ended_at=NULL,
                last_error='create-never-acknowledged' WHERE id=?1",
            rusqlite::params![attested.instance_id],
        )
        .unwrap();
        drop(db);

        let changed = store.expire_lost_hosts(0).await.unwrap().0;
        assert_eq!(
            changed, 0,
            "requested and attested-failed rows are not swept"
        );

        let req = store
            .get_instance(requested.instance_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(req.lifecycle, "requested", "requested row untouched");
        let failed = store
            .get_instance(attested.instance_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.lifecycle, "failed");
        assert_eq!(
            failed.last_error.as_deref(),
            Some("create-never-acknowledged")
        );
        store.close().await;
    }

    // ----- c-cardsettle: settle a session's pending cards when it ends -----

    /// c-cardsettle (Node epoch change / restart reconcile): the lost
    /// instance's still-pending interaction is invalidated with
    /// `resolution.reason = generation-ended`; the survivor's card stays
    /// pending — and this commits with the instance UPDATE.
    #[tokio::test]
    async fn epoch_reconcile_invalidates_pending_interactions_of_lost_instances() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-node").await;
        store
            .record_node_epoch(host.clone(), Some("epoch_a".into()))
            .await
            .expect("first epoch");
        store
            .record_node_epoch(host.clone(), Some("epoch_b".into()))
            .await
            .expect("restart epoch");

        let kept = seed_acknowledged_instance(&store, &host).await;
        let lost = seed_acknowledged_instance(&store, &host).await;
        let kept_int = seed_pending_interaction(&store, &host, &kept.instance_id).await;
        let lost_int = seed_pending_interaction(&store, &host, &lost.instance_id).await;

        let (reconciled, settlement) = store
            .reconcile_reported_instances(
                host.clone(),
                vec![kept.instance_id.clone()],
                "node-epoch-changed".into(),
                true,
            )
            .await
            .expect("reconcile");
        assert_eq!(reconciled, vec![lost.instance_id.clone()]);
        // The settlement returned to callers names exactly the lost card.
        let lost_settled: Vec<(String, String)> = settlement
            .interactions
            .iter()
            .map(|settled| (settled.instance_id.clone(), settled.interaction_id.clone()))
            .collect();
        assert_eq!(
            lost_settled,
            vec![(lost.instance_id.clone(), lost_int.clone())]
        );
        assert!(!settlement.interactions[0].updated_at.is_empty());

        // The instance change and the card settlement are one committed fact.
        let lost_row = store
            .get_instance(lost.instance_id.clone())
            .await
            .expect("get instance")
            .expect("row");
        assert_eq!(lost_row.lifecycle, "exited");
        let (lost_state, lost_reason) = interaction_state_and_reason(&store, &lost_int).await;
        assert_eq!(
            lost_state, "invalidated",
            "lost instance's card is invalidated"
        );
        assert_eq!(
            lost_reason.as_deref(),
            Some("generation-ended"),
            "protocol terminal for a generation that ended"
        );
        let (kept_state, _) = interaction_state_and_reason(&store, &kept_int).await;
        assert_eq!(kept_state, "pending", "the survivor's card stays pending");

        // The invalidated row leaves the actionable query but stays in the
        // inbox feed (recently departed).
        let pending: Vec<_> = store
            .list_interactions(None, None, None, true)
            .await
            .expect("pending only")
            .into_iter()
            .map(|r| r.interaction_id)
            .collect();
        assert!(!pending.contains(&lost_int));
        assert!(pending.contains(&kept_int));
        let inbox: Vec<_> = store
            .list_inbox_interactions(None, None, None)
            .await
            .expect("inbox feed")
            .into_iter()
            .map(|r| r.interaction_id)
            .collect();
        assert!(
            inbox.contains(&lost_int),
            "departed row still feeds the inbox"
        );
        store.close().await;
    }

    /// c-cardsettle: reconcile is idempotent — a second reconcile touches
    /// neither the already-terminal instance nor the settled card, and no
    /// settlement is returned.
    #[tokio::test]
    async fn epoch_reconcile_settling_interactions_is_idempotent() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-idem").await;
        let lost = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &lost.instance_id).await;

        let (lost_first, first) = store
            .reconcile_reported_instances(host.clone(), vec![], "node-epoch-changed".into(), true)
            .await
            .expect("first reconcile");
        assert_eq!(lost_first, vec![lost.instance_id.clone()]);
        assert_eq!(first.interactions.len(), 1);
        // Second reconcile: the instance is already exited, returns no new lost
        // rows, no settlement, and does not error on the terminal interaction.
        let (again, second) = store
            .reconcile_reported_instances(host.clone(), vec![], "node-epoch-changed".into(), true)
            .await
            .expect("second reconcile");
        assert!(again.is_empty());
        assert!(second.is_empty(), "no card is settled twice");
        let (state, _) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated");
        store.close().await;
    }

    /// c-cardsettle (explicit kill / delete / node-lost stop):
    /// settle_instance_exited invalidates pending interactions in the same
    /// change and returns them for broadcast.
    #[tokio::test]
    async fn settle_instance_exited_invalidates_its_pending_interactions() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-stop").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;

        let (changed, settlement) = store
            .settle_instance_exited(instance.instance_id.clone(), "deleted-by-operator".into())
            .await
            .expect("settle");
        assert!(changed);
        assert_eq!(settlement.interactions.len(), 1, "one card settled");
        assert_eq!(settlement.interactions[0].instance_id, instance.instance_id);
        assert_eq!(settlement.interactions[0].interaction_id, int_id);
        assert!(!settlement.interactions[0].updated_at.is_empty());
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(row.lifecycle, "exited");
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated");
        assert_eq!(reason.as_deref(), Some("generation-ended"));

        // A second settle changes nothing and settles nothing.
        let (changed_again, settlement_again) = store
            .settle_instance_exited(instance.instance_id, "deleted-by-operator".into())
            .await
            .expect("re-settle");
        assert!(!changed_again);
        assert!(settlement_again.is_empty());
        store.close().await;
    }

    /// c-cardsettle (journaled exit event): the lifecycle that moves a running
    /// instance to exited invalidates pending interactions on the same journal
    /// connection; the returned append carries the settlement for broadcast.
    #[tokio::test]
    async fn journaled_exit_lifecycle_invalidates_pending_interactions() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-exit").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;

        let appended = store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{"type":"entity","entityType":"instance","state":"exited"}}),
            )
            .await
            .expect("append exit");
        assert_eq!(appended.settlement.interactions.len(), 1);
        assert_eq!(
            appended.settlement.interactions[0].instance_id,
            instance.instance_id
        );
        assert_eq!(
            appended.settlement.interactions[0].interaction_id, int_id,
            "the journal append reports the card it settled"
        );
        assert!(!appended.settlement.interactions[0].updated_at.is_empty());
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated");
        assert_eq!(reason.as_deref(), Some("generation-ended"));

        // A non-terminal status later cannot un-invalidate (a dead generation
        // stays dead), and it settles nothing further.
        let stray = store
            .append_journal(
                host,
                instance.instance_id,
                None,
                json!({"kind":"lifecycle","payload":{"type":"native","nativeName":"agent_status","status":{"state":"known","value":"working"}}}),
            )
            .await
            .expect("append stray status");
        assert!(stray.settlement.is_empty());
        let (state, _) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(
            state, "invalidated",
            "a stray status never revives the card"
        );
        store.close().await;
    }

    /// c-cardsettle r2 item 1: a failed LIVE configure switch
    /// (instance.configure / topic configuration / affectsCompletion=false,
    /// severity=error) on a still-running PTY session must NOT end the session:
    /// the instance stays running, its pending card stays pending, and no
    /// settlement is returned.
    #[tokio::test]
    async fn live_configure_error_keeps_the_instance_running_and_the_card_pending() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-configure").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;

        for (outcome, detail) in [
            ("model-control-unavailable", "model switch failed"),
            ("effort-control-unavailable", "effort switch failed"),
            ("permission-control-unavailable", "permission switch failed"),
        ] {
            let appended = store
                .append_journal(
                    host.clone(),
                    instance.instance_id.clone(),
                    None,
                    json!({"kind":"lifecycle","payload":{
                        "type":"native",
                        "topic":"configuration",
                        "nativeName":"instance.configure",
                        "status":{"state":"known","value":format!("{outcome}:{detail}")},
                        "severity":"error",
                        "affectsCompletion":false,
                        "relatedIds":{}
                    }}),
                )
                .await
                .expect("append configure error");
            assert!(
                appended.settlement.is_empty(),
                "a configure error settles no cards ({outcome})"
            );
        }
        // r3 item 1: even a native status LITERALLY reporting "failed"/"error"
        // on a non-completion configuration observation must not map the row to
        // failed (the herdr-status mapping shared that bug with the
        // start-failure classifier).
        for status_value in ["failed", "error"] {
            let appended = store
                .append_journal(
                    host.clone(),
                    instance.instance_id.clone(),
                    None,
                    json!({"kind":"lifecycle","payload":{
                        "type":"native",
                        "topic":"configuration",
                        "nativeName":"instance.configure",
                        "status":{"state":"known","value":status_value},
                        "severity":"error",
                        "affectsCompletion":false,
                        "relatedIds":{}
                    }}),
                )
                .await
                .expect("append literal failed status");
            assert!(
                appended.settlement.is_empty(),
                "a non-completion {status_value} status settles no cards"
            );
        }
        // And affectsCompletion=false alone (different topic) is equally
        // non-terminal.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"native",
                    "topic":"session",
                    "nativeName":"transient_runtime_error",
                    "status":{"state":"known","value":"error"},
                    "severity":"error",
                    "affectsCompletion":false,
                    "relatedIds":{}
                }}),
            )
            .await
            .expect("append non-completion session error");

        let row = store
            .get_instance(instance.instance_id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            row.lifecycle, "running",
            "a configure error never folds a live session to failed"
        );
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "pending", "the hook's pending card stays answerable");
        assert!(
            reason.is_none(),
            "no generation-ended resolution is stamped"
        );
        store.close().await;
    }

    /// c-cardsettle r3 addendum (three-way split):
    ///  (a) subagent / workflow-member / configure / diagnostic failures stay in
    ///      their OWN scope — they do not touch the root turn/activity and
    ///      never the lifecycle;
    ///  (b) a ROOT-session failure (StopFailure with no agentId, or a root API
    ///      error ending the root turn) ends the root TURN failed (status/idle,
    ///      composer retryable) but the process stays alive — lifecycle not
    ///      terminal;
    ///  (c) only process-end evidence (native exit/exit code, process/PTY
    ///      gone, launch never started, Node says gone) is lifecycle terminal.
    #[test]
    fn non_process_failure_signals_are_turn_level_not_terminal() {
        // (label, payload json; derived lifecycle must not be failed)
        let cases: Vec<(&str, Value)> = vec![
            // Failed live configure switch (model/effort/permission).
            (
                "configure-error",
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"configuration","nativeName":"instance.configure",
                    "severity":"error","affectsCompletion":false,
                    "status":{"state":"known","value":"model-control-unavailable:x"}}}),
            ),
            // The stop button failed on the MAIN session: turn ends failed,
            // process alive.
            (
                "main-stop-failure",
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"StopFailure",
                    "severity":"warning","affectsCompletion":false,
                    "status":{"state":"known","value":"idle"},
                    "relatedIds":{"outcome":"failed","phase":"turn-ended"}}}),
            ),
            // The exact owner-reported bug: a workflow SUBAGENT's StopFailure
            // (agentId + agentType) ends the subagent's turn, never the main
            // instance.
            (
                "subagent-stop-failure",
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"StopFailure",
                    "severity":"warning","affectsCompletion":false,
                    "status":{"state":"known","value":"idle"},
                    "relatedIds":{
                        "agentId":"agent0sub0agent000","agentType":"workflow-subagent",
                        "outcome":"failed","phase":"turn-ended"}}}),
            ),
            // severity=error API/hook diagnostic.
            (
                "error-diagnostic",
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"diagnostic","nativeName":"api_error",
                    "severity":"error","affectsCompletion":false,
                    "status":{"state":"known","value":"rate limited"}}}),
            ),
            // hook-topic error.
            (
                "hook-error",
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"hook","nativeName":"hook_failed",
                    "severity":"error","affectsCompletion":false,
                    "status":{"state":"known","value":"error"}}}),
            ),
            // r5 item 4: a ROOT topic=turn result error ENDS THE TURN
            // (idle) but not the process; asserted separately below alongside
            // the root StopFailure. Excluded from the own-scope cases loop.
            // Subagent lifecycle marker on session topic, severity error.
            (
                "subagent-session-error",
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"session","nativeName":"error",
                    "severity":"error","affectsCompletion":true,
                    "status":{"state":"known","value":"error"},
                    "relatedIds":{"agentId":"x","agentType":"workflow-subagent"}}}),
            ),
            // r4 item 3: subagent identified by agentId ALONE (no agentType).
            (
                "subagent-no-agent-type",
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"StopFailure",
                    "severity":"warning","affectsCompletion":false,
                    "status":{"state":"known","value":"idle"},
                    "relatedIds":{"agentId":"agentwithouttype"}}}),
            ),
            // r5 item 3: the exact owner replay row — shell-pty hook
            // StopFailure for a subagent, remudaActivity=idle, agentId +
            // agentType. The idle is the SUBAGENT's turn; the root must not
            // idle (and of course must not end).
            (
                "subagent-stop-failure-remuda-activity-idle",
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"StopFailure",
                    "severity":"warning","affectsCompletion":false,
                    "status":{"state":"known","value":"idle"},
                    "relatedIds":{
                        "agentId":"agent0sub0agent000",
                        "agentType":"workflow-subagent",
                        "outcome":"failed","phase":"turn-ended",
                        "remudaActivity":"idle"}}}),
            ),
        ];
        // r4 item 1: for the full three-way split, assert lifecycle AND
        // activity. Own-scope (subagent/configure/diagnostic): BOTH untouched.
        for (label, event) in cases {
            let (lifecycle, activity) = derive_instance_state(&event);
            assert_ne!(
                lifecycle,
                Some("failed"),
                "{label} is turn-level/own-scope and must not mark failed"
            );
            assert_ne!(lifecycle, Some("exited"), "{label} must not mark exited");
            // Subagent/configure/diagnostic events must not set activity either;
            // only a ROOT StopFailure sets idle (checked separately below).
            if label != "main-stop-failure" {
                assert!(
                    activity.is_none(),
                    "{label} (own-scope) must not change root activity, got {activity:?}"
                );
            }
        }
        // A ROOT StopFailure (no agentId) ends the TURN: activity idle but
        // lifecycle stays running.
        let root_stop = json!({"kind":"lifecycle","payload":{
            "type":"native","topic":"turn","nativeName":"StopFailure",
            "severity":"warning","affectsCompletion":false,
            "status":{"state":"known","value":"idle"},
            "relatedIds":{"outcome":"failed","phase":"turn-ended"}}});
        let (root_life, root_act) = derive_instance_state(&root_stop);
        assert_eq!(
            root_life,
            Some("running"),
            "root StopFailure keeps lifecycle running"
        );
        assert_eq!(
            root_act,
            Some("idle"),
            "root StopFailure ends the turn (idle), retryable"
        );

        // r5 item 4: a ROOT print/SDK topic=turn result error also ends the
        // TURN (idle), lifecycle stays running — a failed turn is retryable.
        let root_result_error = json!({"kind":"lifecycle","payload":{
            "type":"native","topic":"turn","nativeName":"result",
            "affectsCompletion":true,
            "status":{"state":"known","value":"error"},
            "relatedIds":{"resultIndex":"1","numTurns":"1"}}});
        let (r_life, r_act) = derive_instance_state(&root_result_error);
        assert_eq!(
            r_life,
            Some("running"),
            "root result error keeps lifecycle running"
        );
        assert_eq!(
            r_act,
            Some("idle"),
            "root result error ends the turn (idle)"
        );
        // A SUBAGENT result error stays own-scope (no idle).
        let sub_result = json!({"kind":"lifecycle","payload":{
            "type":"native","topic":"turn","nativeName":"result",
            "status":{"state":"known","value":"error"},
            "relatedIds":{"agentId":"a1"}}});
        let (s_life, s_act) = derive_instance_state(&sub_result);
        assert_eq!(s_life, None);
        assert_eq!(
            s_act, None,
            "a subagent result error never sets root activity"
        );

        // The signals that DO end it via the DERIVED start-fail/entity rules.
        // (severity=error native exits are projected as failed by
        // apply_instance_projection; native_terminal_projection_... covers that
        // path with a real topic=session exit.)
        let terminal_cases: Vec<(&str, Value)> = vec![
            (
                "native-driver-start-failed",
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"session","nativeName":"x",
                    "severity":"error","affectsCompletion":true,
                    "reasonCode":"native-driver-start-failed",
                    "status":{"state":"known","value":"error"}}}),
            ),
            // Real topic=session native exits (exit/gone/error/severity)
            // are projected to failed by apply_instance_projection, tested
            // by native_terminal_projection_invalidates_... below — not by
            // derive (which only recognises start-fail/entity).
            (
                "entity-failed",
                json!({"kind":"lifecycle","payload":{
                    "type":"entity","entityType":"instance","state":"failed"}}),
            ),
        ];
        for (label, event) in terminal_cases {
            let (lifecycle, _) = derive_instance_state(&event);
            assert!(
                matches!(lifecycle, Some("failed") | Some("exited")),
                "{label} is real process-end evidence and must be terminal, got {lifecycle:?}"
            );
        }
    }

    /// c-cardsettle r3 item 8 / r4 item 4: replay the scrubbed owner evidence
    /// (a workflow subagent's failed stop plus SubagentStart/Stop markers)
    /// through the durable Hub projection. The main instance stays running
    /// with its card pending; nothing settles. Appends AFTER the durable
    /// watermark (not at Some(2) which collides with the seeded ready seq).
    #[tokio::test]
    async fn replayed_subagent_stopfailure_does_not_end_the_main_instance() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-subagent").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;
        // r5 item 3: seed the root as actively WORKING (not just ready), so we
        // can prove a subagent StopFailure with remudaActivity=idle does not
        // flip the root's activity.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"agent_status",
                    "status":{"state":"known","value":"working"}}}),
            )
            .await
            .expect("seed working");
        // Capture the pre-fixture lifecycle AND activity.
        let before = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get before")
            .expect("row");
        assert_eq!(before.lifecycle, "running");
        assert_eq!(before.activity, "working", "seeded root is working");
        let watermark = store
            .read_journal(instance.instance_id.clone(), 0, None)
            .await
            .expect("watermark")
            .durable_seq;
        assert_eq!(watermark, 3, "ready + interaction + working occupy seq 1-3");

        let events: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/subagent-hook-events.json"))
                .expect("fixture parses");
        // Append after the watermark so the StopFailure is NOT discarded as a
        // replay of the existing seq-2 row.
        let first_fixture_seq = events
            .first()
            .and_then(|e| e.get("seq"))
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<i64>().ok())
            .expect("fixture carries seq values");
        assert!(
            first_fixture_seq > watermark,
            "fixture must follow the seed watermark"
        );
        store
            .append_journal_batch(host.clone(), instance.instance_id.clone(), None, events)
            .await
            .expect("replay evidence");

        // Prove the StopFailure event was actually committed (not discarded as
        // a replay): the fixture appends after the watermark, and the store
        // gap-fills the non-contiguous fixture seqs (1981…) to gapless durable
        // seqs (3..). The first committed event must be the subagent
        // StopFailure content at durable seq watermark+1.
        let page = store
            .read_journal(instance.instance_id.clone(), watermark, None)
            .await
            .expect("read committed StopFailure");
        let first_committed = page
            .events
            .iter()
            .find(|e| e.seq == watermark + 1)
            .expect("an event at the seq after the watermark");
        let native_name = first_committed
            .event
            .pointer("/payload/nativeName")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(
            native_name, "StopFailure",
            "the first post-watermark event is the subagent StopFailure, not discarded"
        );

        // Assert the post-state EQUALS the captured pre-state literally.
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get after")
            .expect("row");
        assert_eq!(
            row.lifecycle, before.lifecycle,
            "lifecycle unchanged: a subagent StopFailure never ends the root"
        );
        assert_eq!(
            row.activity, before.activity,
            "activity unchanged: stays working (a subagent remudaActivity=idle never idles the root)"
        );
        let (state, _) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "pending", "the main session's card stays pending");
        store.close().await;
    }

    /// c-cardsettle r5 item 8: the real driver demotion retirement
    /// (shell_pty promotion `invalidate_picker` → entity lifecycle
    /// entityType=interaction, state=invalidated, reasonCode=agent-demoted)
    /// replays as agent-demoted — on the live row, in the lag/snapshot
    /// recovery, and on the delete-tombstone — never mislabelled
    /// generation-ended.
    #[tokio::test]
    async fn a_demotion_replays_as_agent_demoted_not_generation_ended() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "demotion-reason").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;

        // The exact shell_pty retire_payload("invalidated", "agent-demoted")
        // entity lifecycle: the producer supplies reasonCode but the
        // Interaction entity's resolution is still UNKNOWN (r6 item 4).
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({
                    "kind": "lifecycle",
                    "payload": {
                        "type": "entity",
                        "entityType": "interaction",
                        "entityId": int_id,
                        "revision": "3",
                        "previousState": "pending",
                        "state": "invalidated",
                        "reasonCode": "agent-demoted",
                        "evidenceEventIds": [],
                        "entity": {
                            "id": int_id,
                            "state": "invalidated",
                            "blocking": false,
                            "answerable": false,
                            "resolution": { "state": "unknown" }
                        }
                    }
                }),
            )
            .await
            .expect("demotion replay");

        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated");
        assert_eq!(
            reason.as_deref(),
            Some("agent-demoted"),
            "a demotion with an Unknown entity resolution still stores its reasonCode"
        );

        // The snapshot/lag recovery carries the demotion reason through.
        let recent = store
            .recent_invalidated_interactions()
            .await
            .expect("recent");
        let recovered = recent
            .iter()
            .find(|(_, id, _)| id == &int_id)
            .expect("demotion row in recovery");
        assert_eq!(recovered.2, "agent-demoted");

        // Deleting the instance tombstones the row WITH its real reason (the
        // already-invalidated demotion row is not re-stamped by the settle).
        let (_, _) = store
            .settle_instance_exited(instance.instance_id.clone(), "x".into())
            .await
            .expect("settle");
        assert!(
            store
                .delete_instance(instance.instance_id.clone())
                .await
                .expect("delete")
        );
        let page = store
            .invalidated_interactions_after(None)
            .await
            .expect("cursor page");
        let tomb = page
            .iter()
            .find(|(_, id, _, _)| id == &int_id)
            .expect("tombstone recovered");
        assert_eq!(
            tomb.2, "agent-demoted",
            "the tombstone labels a demotion as agent-demoted, not generation-ended"
        );
        store.close().await;
    }

    /// c-cardsettle r6 item 1: the production drivers' STARTUP frames share
    /// the exit nativeName (`topic=session`, nativeName `session`) — print/SDK
    /// init status "started", PTY ready statuses idle/working/blocked/done/
    /// unknown. Journaled through the Hub they must keep the instance running
    /// and the approval pending; only the later real `session/exited` settles.
    #[tokio::test]
    async fn startup_session_frames_keep_the_instance_running_until_the_real_exit() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "startup-frames-not-exit").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;

        // The print/SDK mapper init frame.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","observedAt":"2026-10-07T10:00:00.000Z","payload":{
                    "type":"native","topic":"session","nativeName":"session",
                    "severity":"info","affectsCompletion":false,
                    "nativeId":{"state":"known","value":"sess-1"},
                    "status":{"state":"known","value":"started"}}}),
            )
            .await
            .expect("print init");
        // The Claude/generic PTY ready frames with every live agent status,
        // including one carrying an error severity (still not an end).
        for (status, severity) in [
            ("idle", "info"),
            ("working", "info"),
            ("blocked", "info"),
            ("done", "info"),
            ("unknown", "info"),
            ("working", "error"),
        ] {
            store
                .append_journal(
                    host.clone(),
                    instance.instance_id.clone(),
                    None,
                    json!({"kind":"lifecycle","payload":{
                        "type":"native","topic":"session","nativeName":"session",
                        "severity":severity,"affectsCompletion":false,
                        "nativeId":{"state":"known","value":"pane-1"},
                        "status":{"state":"known","value":status}}}),
                )
                .await
                .expect("pty ready");
        }
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            row.lifecycle, "running",
            "startup frames on the session name never end the instance"
        );
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "pending");
        assert!(reason.is_none());

        // The REAL print session/exited then ends it and settles the card.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","observedAt":"2026-10-07T10:05:00.000Z","payload":{
                    "type":"native","topic":"session","nativeName":"session",
                    "severity":"info","affectsCompletion":false,
                    "nativeId":{"state":"known","value":"sess-1"},
                    "status":{"state":"known","value":"exited"}}}),
            )
            .await
            .expect("real exit");
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(row.lifecycle, "exited");
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated");
        assert_eq!(reason.as_deref(), Some("generation-ended"));
        store.close().await;
    }

    /// c-cardsettle r5 item 4 (OA6): the print/SDK mapper's REAL root turn
    /// failure (claude_print `map_result`: topic=turn, nativeName=result,
    /// status=error, resultIndex/numTurns) appended through the Hub ENDS THE
    /// TURN — activity idle, outcome retryable — while lifecycle stays
    /// running and the pending approval is NOT settled. A subagent result
    /// error changes nothing. A real session exit afterwards still settles.
    #[tokio::test]
    async fn a_root_result_error_ends_the_turn_but_keeps_the_session_live() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-result-error").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"agent_status",
                    "status":{"state":"known","value":"working"}}}),
            )
            .await
            .expect("seed working");

        // The exact claude_print map_result output for an errored turn.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"result",
                    "severity":"info",
                    "nativeId":{"state":"known","value":"sess-1"},
                    "status":{"state":"known","value":"error"},
                    "affectsCompletion":true,
                    "relatedIds":{"resultIndex":"1","numTurns":"1"}}}),
            )
            .await
            .expect("append root result error");
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            row.lifecycle, "running",
            "a root result error is turn-level: the process stays alive"
        );
        assert_eq!(
            row.activity, "idle",
            "a root result error ends the turn (composer idle, retryable)"
        );
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "pending", "the approval stays answerable");
        assert!(reason.is_none(), "no settlement on a turn failure");

        // A SUBAGENT result error must not even idle the root; a new working
        // edge brings the root back to working first.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"agent_status",
                    "status":{"state":"known","value":"working"}}}),
            )
            .await
            .expect("working again");
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"result",
                    "status":{"state":"known","value":"error"},
                    "relatedIds":{"agentId":"a1","resultIndex":"2"}}}),
            )
            .await
            .expect("append subagent result error");
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            row.activity, "working",
            "a subagent result error never idles the root"
        );
        assert_eq!(row.lifecycle, "running");

        // The real session exit still settles the card.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"session","nativeName":"session",
                    "severity":"error","affectsCompletion":true,
                    "status":{"state":"known","value":"failed"},
                    "relatedIds":{"lastError":"pane exited"}}}),
            )
            .await
            .expect("append session exit");
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(row.lifecycle, "failed", "the real process exit is terminal");
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated", "the card settles on the real end");
        assert_eq!(reason.as_deref(), Some("generation-ended"));
        store.close().await;
    }

    /// c-cardsettle r5 item 1 (the owner's bug): feed the REAL
    /// WorkflowJournalTailer agent_failed output through the Hub — both the
    /// synthesized `workflow.member` observation (kind=workflow.member,
    /// payload.state=failed) and, defensively, the same fact carried as a
    /// lifecycle ENTITY with entityType=workflow.member. Root lifecycle and
    /// activity must not move, its approval must stay pending, and the
    /// member-failed observations must be retained verbatim in the journal
    /// (the member IS failed — that row is just not the root's row).
    #[tokio::test]
    async fn a_failed_workflow_member_never_ends_the_root() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-member-failed").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;
        // Root is actively WORKING with a pending approval.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"turn","nativeName":"agent_status",
                    "status":{"state":"known","value":"working"}}}),
            )
            .await
            .expect("seed working");
        let before = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get before")
            .expect("row");
        assert_eq!(before.lifecycle, "running");
        assert_eq!(before.activity, "working");

        // 1) The REAL tailer output: kind=workflow.member, state failed.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({
                    "kind": "workflow.member",
                    "payload": {
                        "workflowId": "wf_owner",
                        "memberId": "wf_owner:agent:agent0sub0agent000",
                        "nativeAgentId": {"state":"known","value":"agent0sub0agent000"},
                        "nativeKey": {"state":"unknown","reason":"not-emitted"},
                        "attempt": {"state":"known","value":"1"},
                        "label": {"state":"known","value":"researcher"},
                        "state": "failed",
                        "revision": "7",
                    }
                }),
            )
            .await
            .expect("append real workflow.member failed");
        // 2) The same fact as a lifecycle ENTITY (entityType
        //    workflow.member): must be rejected as a root lifecycle source.
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"entity","entityType":"workflow.member",
                    "state":"failed","reasonCode":"member-stop-failed",
                    "memberId":"wf_owner:agent:agent0sub0agent000"}}),
            )
            .await
            .expect("append workflow.member entity failed");

        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get after")
            .expect("row");
        assert_eq!(
            row.lifecycle, "running",
            "a failed WORKFLOW MEMBER never fails the root"
        );
        assert_eq!(
            row.activity, "working",
            "a failed WORKFLOW MEMBER never idles the root"
        );
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(
            state, "pending",
            "the root's pending approval stays answerable"
        );
        assert!(
            reason.is_none(),
            "no generation-ended resolution is stamped"
        );

        // The member-failed evidence itself is retained in the journal.
        let page = store
            .read_journal(instance.instance_id.clone(), 0, None)
            .await
            .expect("read journal");
        let member_failed = page.events.iter().any(|record| {
            record.event.get("kind").and_then(Value::as_str) == Some("workflow.member")
                && record
                    .event
                    .pointer("/payload/state")
                    .and_then(Value::as_str)
                    == Some("failed")
                && record
                    .event
                    .pointer("/payload/nativeAgentId/value")
                    .and_then(Value::as_str)
                    == Some("agent0sub0agent000")
        });
        assert!(
            member_failed,
            "the member IS failed: its workflow.member observation is retained verbatim"
        );
        store.close().await;
    }

    /// c-cardsettle (native terminal projection): a NATIVE terminal event —
    /// a real process death (`topic=session`, nativeName "exit",
    /// affectsCompletion=true, severity "error") — writes lifecycle=failed
    /// through apply_instance_projection even though the derived lifecycle
    /// does not name it; the pending card must settle on that transition.
    #[tokio::test]
    async fn native_terminal_projection_invalidates_pending_interactions() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-native-fail").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;

        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                None,
                json!({"kind":"lifecycle","payload":{
                    "type":"native","topic":"session","nativeName":"exit",
                    "severity":"error","affectsCompletion":true,
                    "status":{"state":"known","value":"pane exited; agent process is gone"},
                    "relatedIds":{"lastError":"boom"}
                }}),
            )
            .await
            .expect("append native exit");

        let row = store
            .get_instance(instance.instance_id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            row.lifecycle, "failed",
            "the native projection fails the instance"
        );
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated", "native terminal ends the generation");
        assert_eq!(reason.as_deref(), Some("generation-ended"));
        store.close().await;
    }

    /// c-cardsettle (never-acknowledged create): a `requested` instance never
    /// journaled a card (journaling interaction.requested transitions it to
    /// running), so the reaper finds no pending interactions — the defensive
    /// in-transaction settle has nothing to do and the instance still fails.
    #[tokio::test]
    async fn expire_stale_requested_has_no_pending_interactions_and_fails() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-stale").await;
        let instance = seed_instance(&store, &host).await; // stays `requested`
        backdate_instance(&store, &instance.instance_id, 60).await;

        let (expired, settlement) = store
            .expire_stale_requested(REQUESTED_SLOT_WINDOW_MS)
            .await
            .expect("sweep");
        assert_eq!(expired, vec![(host.clone(), instance.instance_id.clone())]);
        assert!(
            settlement.is_empty(),
            "a never-launched instance never raised a card"
        );
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(row.lifecycle, "failed");
        assert!(
            store
                .list_interactions(None, Some(instance.instance_id.clone()), None, false)
                .await
                .expect("list")
                .is_empty(),
            "a never-launched instance never raised a card"
        );
        store.close().await;
    }

    /// c-cardsettle (host-lost sweep): expire_lost_hosts settles the instances
    /// of an unreachable host and invalidates their still-pending interactions
    /// in the same transaction.
    #[tokio::test]
    async fn expire_lost_hosts_invalidates_pending_interactions() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-hostlost").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;

        store
            .mark_host_offline(host.clone())
            .await
            .expect("offline");
        let (swept, settlement) = store.expire_lost_hosts(0).await.expect("host-lost sweep");
        assert_eq!(swept, 1);
        assert_eq!(settlement.interactions.len(), 1, "one card settled");
        assert_eq!(settlement.interactions[0].instance_id, instance.instance_id);
        assert_eq!(settlement.interactions[0].interaction_id, int_id);
        assert!(!settlement.interactions[0].updated_at.is_empty());
        let row = store
            .get_instance(instance.instance_id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(row.lifecycle, "exited");
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated");
        assert_eq!(reason.as_deref(), Some("generation-ended"));
        store.close().await;
    }

    /// c-cardsettle (launch rejected): fail_instance invalidates any pending
    /// interaction on the row it moves to failed.
    #[tokio::test]
    async fn fail_instance_invalidates_its_pending_interactions() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-fail").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;

        let settlement = store
            .fail_instance(instance.instance_id.clone(), "node rejected launch".into())
            .await
            .expect("fail");
        assert_eq!(settlement.interactions.len(), 1, "one card settled");
        assert_eq!(settlement.interactions[0].instance_id, instance.instance_id);
        assert_eq!(settlement.interactions[0].interaction_id, int_id);
        assert!(!settlement.interactions[0].updated_at.is_empty());
        let row = store
            .get_instance(instance.instance_id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(row.lifecycle, "failed");
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated");
        assert_eq!(reason.as_deref(), Some("generation-ended"));
        store.close().await;
    }

    /// c-cardsettle r2 item 2: deleting a terminal instance removes the live
    /// interaction rows but retains their terminal state as tombstones, so a
    /// late answer can be rejected without fanning out to Nodes.
    #[tokio::test]
    async fn delete_instance_retains_terminal_interactions_as_tombstones() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-tombstone").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;

        let (changed, _) = store
            .settle_instance_exited(instance.instance_id.clone(), "deleted-by-operator".into())
            .await
            .expect("settle");
        assert!(changed);
        assert!(
            store
                .delete_instance(instance.instance_id.clone())
                .await
                .expect("delete")
        );

        // The live row is gone with the instance…
        assert!(
            store
                .get_interaction(int_id.clone())
                .await
                .expect("get")
                .is_none(),
            "the interaction row is deleted with the instance"
        );
        // …but the terminal state survives as a tombstone.
        let tombstone = store
            .get_interaction_tombstone(int_id.clone())
            .await
            .expect("get tombstone")
            .expect("tombstone retained");
        assert_eq!(tombstone.interaction_id, int_id);
        assert_eq!(tombstone.instance_id, instance.instance_id);
        assert_eq!(tombstone.state, "invalidated");
        // r3 item 2: the tombstone is authoritative in the merge dedup, so a
        // Node that still lists the id pending cannot re-queue it post-delete.
        let terminal = store
            .terminal_interaction_ids(vec![int_id.clone()])
            .await
            .expect("terminal ids");
        assert!(terminal.contains(&int_id), "tombstone id is dedup-terminal");
        store.close().await;
    }

    /// c-cardsettle r2 item 6: replaying an `interaction.requested` (e.g. after
    /// a lost ack) must not overwrite an already-invalidated/expired row's
    /// payload or strip its generation-ended resolution, and never revives the
    /// instance's blocked activity.
    #[tokio::test]
    async fn replayed_requested_keeps_a_terminal_rows_payload_and_state() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-replay").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;
        let (_, _) = store
            .settle_instance_exited(instance.instance_id.clone(), "deleted-by-operator".into())
            .await
            .expect("settle");
        let (state, reason) = interaction_state_and_reason(&store, &int_id).await;
        assert_eq!(state, "invalidated");
        assert_eq!(reason.as_deref(), Some("generation-ended"));

        // A lost-ack replay of the SAME interaction.requested event: append it
        // again with the same explicit seq (an existing journal row replays
        // instead of inserting).
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                Some(2),
                json!({
                    "kind": "interaction.requested",
                    "payload": {
                        "interactionKind": "approval",
                        "interaction": {
                            "id": int_id,
                            "kind": "approval",
                            "state": "pending",
                            "blocking": true,
                            "answerable": true,
                            "carrier": "harness-hook",
                            "deadline": { "state": "unknown" },
                            "resolution": { "state": "unknown" },
                            "request": {"kind": "approval", "title": "Replay", "options": []}
                        }
                    }
                }),
            )
            .await
            .expect("replay");

        // State, payload (resolution) and blocking all survive the replay.
        let row = store
            .get_interaction(int_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            row.state, "invalidated",
            "a replay never revives a terminal row"
        );
        assert!(!row.blocking);
        let reason = row
            .payload
            .pointer("/payload/interaction/resolution/value/reason")
            .and_then(Value::as_str);
        assert_eq!(
            reason,
            Some("generation-ended"),
            "the settlement resolution survives the replay (desktop keeps 进程已结束 wording)"
        );
        // A terminal instance is not re-blocked.
        let inst = store
            .get_instance(instance.instance_id)
            .await
            .expect("get instance")
            .expect("row");
        assert_eq!(inst.lifecycle, "exited");
        assert_ne!(inst.activity, "blocked");
        store.close().await;
    }

    /// c-cardsettle r2 item 3: authoritative terminal dedup ignores display
    /// retention — a durable invalidated row is reported terminal even when it
    /// is older than the 24 h inbox window, so a stale Node live copy of the
    /// same id can never re-queue.
    #[tokio::test]
    async fn terminal_interaction_ids_ignores_the_inbox_retention_cutoff() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cardsettle-dedup").await;
        let instance = seed_acknowledged_instance(&store, &host).await;
        let int_id = seed_pending_interaction(&store, &host, &instance.instance_id).await;
        let (_, _) = store
            .settle_instance_exited(instance.instance_id.clone(), "deleted-by-operator".into())
            .await
            .expect("settle");
        // Age the row well past the 24 h departed retention.
        backdate_interaction(&store, &int_id, 48).await;

        // It no longer feeds the inbox page…
        let inbox = store
            .list_inbox_interactions(None, None, None)
            .await
            .expect("inbox");
        assert!(
            !inbox.iter().any(|r| r.interaction_id == int_id),
            "aged row hidden from display"
        );
        // …but authoritative dedup still knows it is terminal.
        let terminal = store
            .terminal_interaction_ids(vec![int_id.clone(), "int_does_not_exist".to_string()])
            .await
            .expect("terminal ids");
        assert!(
            terminal.contains(&int_id),
            "terminal state wins regardless of age"
        );
        assert_eq!(terminal.len(), 1, "unknown ids are not reported terminal");
        store.close().await;
    }

    /// A stop the Node cannot honour settles instead of hanging forever.
    #[tokio::test]
    async fn settling_an_unknown_instance_releases_its_slot_once() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "stop-node").await;
        let instance = seed_instance(&store, &host).await;

        assert!(
            store
                .settle_instance_exited(instance.instance_id.clone(), "node-lost-instance".into())
                .await
                .expect("settle")
                .0
        );
        assert!(
            !store
                .settle_instance_exited(instance.instance_id.clone(), "node-lost-instance".into())
                .await
                .expect("settle again")
                .0,
            "an already-exited row is not re-settled"
        );
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(row.lifecycle, "exited");
        assert_eq!(row.last_error.as_deref(), Some("node-lost-instance"));
        assert_eq!(store.running_count(host).await.expect("count"), 0);

        let diagnostic = store
            .append_hub_diagnostic(
                instance.instance_id.clone(),
                "node_lost_instance".into(),
                "settled as exited".into(),
            )
            .await
            .expect("diagnostic")
            .expect("record");
        assert_eq!(diagnostic.event["payload"]["origin"], json!("hub"));
        assert_eq!(diagnostic.event["payload"]["topic"], json!("diagnostic"));
        let journal_page = store
            .read_journal(instance.instance_id, 0, None)
            .await
            .expect("journal");
        let events = journal_page.events;
        let durable = journal_page.durable_seq;
        assert_eq!(durable, diagnostic.seq);
        assert!(
            events
                .iter()
                .any(|event| event.event["payload"]["nativeName"] == json!("node_lost_instance")),
            "{events:?}"
        );
        store.close().await;
    }

    /// The operator ceiling must outlive Node inventory and a Hub restart.
    #[tokio::test]
    async fn operator_max_instances_override_survives_hello_and_restart() {
        let dir = tempfile::tempdir().expect("dir");
        let host = new_id("hst").expect("host");
        {
            let store = Store::open(dir.path()).expect("store");
            enroll_labeled(&store, host.clone(), "cap-node").await;
            let patched = store
                .patch_host(
                    host.clone(),
                    None,
                    None,
                    Some(32),
                    None,
                    Default::default(),
                    None,
                )
                .await
                .expect("patch");
            assert_eq!(patched.max_instances, 32);

            // A Node hello re-advertising its own ceiling must not undo it.
            let inventory = crate::inventory::from_node_params(&json!({"maxInstances": 8}));
            let after_hello = store
                .apply_inventory(host.clone(), inventory, None)
                .await
                .expect("inventory");
            assert_eq!(
                after_hello.max_instances, 32,
                "node hello must not reset the operator ceiling"
            );
            store.close().await;
        }
        let store = Store::open(dir.path()).expect("reopen");
        let reloaded = store.get_host(host).await.expect("get").expect("host");
        assert_eq!(
            reloaded.max_instances, 32,
            "the override must survive a Hub restart"
        );
        store.close().await;
    }

    async fn enroll_labeled(store: &Store, host_id: String, label: &str) {
        let token = enroll_token(store, &format!("enroll-{host_id}")).await;
        let outcome = store
            .authenticate_host(
                HostAuthRequest {
                    presented: token,
                    hello_host_id: Some(host_id),
                    label: Some(label.to_owned()),
                    node_version: Some("test".into()),
                },
                verify_eq,
                |secret| Ok(format!("hash-{secret}")),
            )
            .await
            .expect("enroll");
        assert!(matches!(outcome, HostAuthOutcome::Authenticated { .. }));
    }
    /// §9.1: an `effort` journal event persists the transcript-read-back level
    /// as `effortEffective`, and never overwrites the requested `effort`.
    #[tokio::test]
    async fn effort_observation_projects_effective_without_touching_requested() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cap-node").await;
        let instance = seed_instance(&store, &host).await;
        store
            .patch_instance_configure(
                instance.instance_id.clone(),
                json!({"effort": {"name": "max", "index": 4}}),
            )
            .await
            .expect("configure");
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                Some(1),
                json!({"kind":"effort","payload":{
                    "requested":{"name":"max","ultracode":false},
                    "effective":{"name":"xhigh","ultracode":null,
                        "source":"slash","observedAt":"2026-09-14T12:00:00.000Z"},
                    "raw":"xhigh"}}),
            )
            .await
            .expect("effort event");
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(row.effort_name.as_deref(), Some("max"));
        let effective = row.effort_effective.expect("effortEffective stored");
        assert_eq!(effective.get("name").and_then(Value::as_str), Some("xhigh"));
        assert_eq!(
            effective.get("source").and_then(Value::as_str),
            Some("slash")
        );
    }

    /// A `model` journal event persists the observed id as `modelEffective`
    /// without the Hub trusting the requested pin.
    #[tokio::test]
    async fn model_observation_projects_effective_id() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cap-node").await;
        let instance = seed_instance(&store, &host).await;
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                Some(1),
                json!({"kind":"model","payload":{
                    "requested":"acme_hub/model_x_o50[1m]",
                    "effective":{"id":"acme_hub/model_x_o48[1m]",
                        "source":"launch","observedAt":"2026-09-18T00:00:00.000Z"},
                    "raw":"acme_hub/model_x_o48[1m]"}}),
            )
            .await
            .expect("model event");
        let row = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row");
        let effective = row.model_effective.expect("modelEffective stored");
        assert_eq!(
            effective.get("id").and_then(Value::as_str),
            Some("acme_hub/model_x_o48[1m]")
        );
    }

    /// A `model_pin_mismatch` diagnostic projects verbatim onto the instance
    /// spec as `modelPinMismatches`, independent of the journal tail window,
    /// and accumulates/de-dupes across re-launches (model-pin-1 §5.4).
    #[tokio::test]
    async fn model_pin_mismatch_projects_onto_instance_record() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cap-node").await;
        let instance = seed_instance(&store, &host).await;
        let event = json!({"kind":"lifecycle","observedAt":"2026-09-24T00:00:00.000Z","payload":{
            "type":"native","topic":"diagnostic","nativeName":"model_pin_mismatch",
            "nativeId":{"state":"not-applicable"},
            "status":{"state":"known","value":"diverged"},
            "severity":"warning","affectsCompletion":false,"dataRef":null,
            "relatedIds":{"reason":"model-mismatch",
                "requested":"passthrough/ark/model-y",
                "observed":"ark/model-y"}}});
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                Some(1),
                event.clone(),
            )
            .await
            .expect("diagnostic");
        // A replay of the same event must not duplicate the projection.
        store
            .append_journal(host.clone(), instance.instance_id.clone(), Some(1), event)
            .await
            .expect("replay");
        // Assert through the PUBLIC instance record (the shape the API
        // serializes), not private spec_json. Replay must not duplicate.
        let mismatches = store
            .get_instance(instance.instance_id.clone())
            .await
            .expect("get")
            .expect("row")
            .model_pin_mismatches
            .expect("projected mismatches on the public record");
        let rows = mismatches.as_array().expect("array");
        assert_eq!(rows.len(), 1, "replay must not duplicate");
        assert_eq!(
            rows[0].get("requested").and_then(Value::as_str),
            Some("passthrough/ark/model-y")
        );
        assert_eq!(
            rows[0].get("observed").and_then(Value::as_str),
            Some("ark/model-y")
        );
        assert_eq!(
            rows[0].get("observedAt").and_then(Value::as_str),
            Some("2026-09-24T00:00:00.000Z")
        );

        // A second, distinct divergence accumulates.
        store
            .append_journal(
                host,
                instance.instance_id.clone(),
                Some(2),
                json!({"kind":"lifecycle","observedAt":"2026-09-24T01:00:00.000Z","payload":{
                    "type":"native","topic":"diagnostic","nativeName":"model_pin_mismatch",
                    "nativeId":{"state":"not-applicable"},
                    "status":{"state":"known","value":"diverged"},
                    "severity":"warning","affectsCompletion":false,"dataRef":null,
                    "relatedIds":{"reason":"model-mismatch","requested":"A2","observed":"B2"}}}),
            )
            .await
            .expect("second diagnostic");
        let count = store
            .get_instance(instance.instance_id)
            .await
            .expect("get2")
            .expect("row2")
            .model_pin_mismatches
            .and_then(|value| value.as_array().map(|rows| rows.len()));
        assert_eq!(count, Some(2));
    }

    /// A `permission` journal event persists the transcript-read-back mode as
    /// `permissionEffective` without the Hub trusting the requested word.
    #[tokio::test]
    async fn permission_observation_projects_effective_mode() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path()).expect("store");
        let host = new_id("hst").expect("host");
        enroll_labeled(&store, host.clone(), "cap-node").await;
        let instance = seed_instance(&store, &host).await;
        store
            .patch_instance_configure(
                instance.instance_id.clone(),
                json!({"permissionMode": "auto"}),
            )
            .await
            .expect("configure");
        store
            .append_journal(
                host.clone(),
                instance.instance_id.clone(),
                Some(1),
                json!({"kind":"permission","payload":{
                    "requested":"auto",
                    "effective":{"mode":"auto","source":"remuda",
                        "observedAt":"2026-09-16T12:00:00.000Z"},
                    "raw":"auto"}}),
            )
            .await
            .expect("permission event");
        let spec: Value = store
            .run_named(
                "permission_observation_projects_effective_mode",
                move |conn| {
                    let raw: String = conn.query_row(
                        "SELECT spec_json FROM instances WHERE id = ?1",
                        params![instance.instance_id.clone()],
                        |row| row.get(0),
                    )?;
                    Ok(serde_json::from_str::<Value>(&raw).unwrap_or(json!({})))
                },
            )
            .await
            .expect("spec");
        assert_eq!(
            spec.get("permissionMode").and_then(Value::as_str),
            Some("auto")
        );
        let effective = spec
            .get("permissionEffective")
            .expect("permissionEffective stored");
        assert_eq!(effective.get("mode").and_then(Value::as_str), Some("auto"));
        assert_eq!(
            effective.get("source").and_then(Value::as_str),
            Some("remuda")
        );
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

fn object_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ObjectRecord> {
    Ok(ObjectRecord {
        object_id: row.get(0)?,
        instance_id: row.get(1)?,
        host_id: row.get(2)?,
        media_type: row.get(3)?,
        stored_name: row.get(4)?,
        original_name: row.get(5)?,
        kind: row.get(6)?,
        digest: row.get(7)?,
        byte_len: row.get(8)?,
        expires_at: row.get(9)?,
        anchor: row.get(10)?,
    })
}

const OBJECT_COLUMNS: &str = "id, instance_id, host_id, media_type, stored_name, original_name, \
                             kind, digest, byte_len, expires_at, anchor";

fn load_object(conn: &Connection, id: &str) -> Result<Option<ObjectRecord>, StoreError> {
    conn.query_row(
        &format!("SELECT {OBJECT_COLUMNS} FROM objects WHERE id = ?1"),
        params![id],
        object_from_row,
    )
    .optional()
    .map_err(StoreError::from)
}

fn load_object_by_digest(
    conn: &Connection,
    instance_id: &str,
    digest: &str,
) -> Result<Option<ObjectRecord>, StoreError> {
    conn.query_row(
        &format!("SELECT {OBJECT_COLUMNS} FROM objects WHERE instance_id = ?1 AND digest = ?2"),
        params![instance_id, digest],
        object_from_row,
    )
    .optional()
    .map_err(StoreError::from)
}

/// RFC3339 UTC `secs` from now.
fn rfc3339_after(secs: i64) -> String {
    let t = time::OffsetDateTime::now_utc() + time::Duration::seconds(secs);
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

/// Stamp the Hub-side sample time onto a Node-reported `resources` object.
///
/// The Hub clock is authoritative: freshness gating placement against the
/// Node's clock would break on clock skew (the hello path already measures
/// and warns about it).
pub(crate) fn stamp_resources_sampled_at(resources: &mut Value, now: &str) {
    if let Some(object) = resources.as_object_mut() {
        object.insert("sampledAt".into(), json!(now));
    }
}

pub(crate) fn load_host(conn: &Connection, id: &str) -> Result<Option<HostRecord>, StoreError> {
    let row = conn
        .query_row(
            "SELECT id, label, state, last_seen_at, node_version, cli_json, capabilities_json, transport,
                    labels_json, herdr_json, resources_json,
                    COALESCE(max_instances_override, max_instances), hostname, os, provider_binding,
                    default_launch_args, claude_binary_path, default_tui, relay_bind_json
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
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, Option<String>>(14)?
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "auto".into()),
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, Option<String>>(17)?,
                    row.get::<_, Option<String>>(18)?,
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
        host_os,
        provider_binding,
        default_launch_args,
        claude_binary_path,
        default_tui,
        relay_bind_json,
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
    let (workspace_revision, workspaces) = crate::workspaces::load_snapshot(conn, id)?;
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
        host_os,
        provider_binding,
        // A column that fails to parse is treated as absent rather than
        // failing the read: a malformed default must not make the host
        // unloadable and the whole fleet view unavailable.
        default_launch_args: default_launch_args
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok()),
        claude_binary_path: claude_binary_path.filter(|value| !value.trim().is_empty()),
        default_tui: default_tui
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok()),
        workspaces,
        workspace_revision: workspace_revision.max(0) as u64,
        relay_bind: relay_bind_json.and_then(|raw| serde_json::from_str(&raw).ok()),
    }))
}

fn load_instance(conn: &Connection, id: &str) -> Result<Option<InstanceRecord>, StoreError> {
    conn.query_row(
        "SELECT id, host_id, workspace_id, kind, driver, lifecycle, activity, connectivity,
                title, journal_id, durable_seq, created_at, updated_at, spec_json, last_error,
                mode, promoted_at, launched_by,
                role, scope_json, grants_json, task_id, api_route_json, configure_seq,
                lineage_id, generation, chapter_cause, fenced_at, restart_json
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
            let provider_source = spec
                .get("providerSource")
                .and_then(Value::as_str)
                .map(str::to_string);
            let provider_source_hint = spec
                .get("providerSourceHint")
                .and_then(Value::as_str)
                .map(str::to_string);
            // D-047: the observed route is its own column (`api_route_json`,
            // read at index 22 above), written only from the Node's create
            // echo. It is deliberately never derived from the spec here: the
            // spec carries the *requested* route, whose `route` may be `auto`,
            // which is not an observation value. A pre-D-047 row has no column
            // value and reads as `None` — a direct session, truthfully.
            let model = spec
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            let permission_mode = spec
                .get("permissionMode")
                .and_then(Value::as_str)
                .map(str::to_string);
            let effort = spec.get("effort");
            // D-028 §9.1: normalize by NAME, never by index, using the shared
            // per-harness normalizer (remuda-protocol). Codex `max` / `ultra`
            // remain native levels; legacy Codex `minimal` and Grok aliases
            // migrate exactly as the driver and the web table do.
            let legacy_name = effort
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .or_else(|| effort.and_then(Value::as_str))
                .or_else(|| spec.get("effortName").and_then(Value::as_str));
            let effort_ultracode = effort
                .and_then(|value| value.get("ultracode"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let effort_kind =
                remuda_protocol::effort_kind_from_str(row.get::<_, String>(3)?.as_str());
            let normalized = legacy_name.map(|name| {
                let mut selection = remuda_protocol::normalize_legacy_effort(effort_kind, name);
                if effort_ultracode {
                    selection.ultracode = true;
                }
                selection
            });
            let effort_name = normalized.map(|selection| selection.level_name().to_string());
            let effort_ultracode = normalized.map(|selection| selection.ultracode);
            let effort_index = effort
                .and_then(|value| value.get("index"))
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .or_else(|| {
                    spec.get("effortIndex")
                        .and_then(Value::as_u64)
                        .map(|n| n as u32)
                });
            let effort_effective = spec.get("effortEffective").cloned();
            let model_effective = spec.get("modelEffective").cloned();
            let model_catalog = spec.get("modelCatalog").cloned();
            let model_pin_mismatches = spec.get("modelPinMismatches").cloned();
            let mode: Option<String> = row.get(15)?;
            let promoted_at: Option<String> = row.get(16)?;
            let stored: Option<String> = row.get(17)?;
            // The stored value wins. The derivation below is only for rows
            // written before the column existed, where `mode == promoted` was
            // still a sound proxy because Remuda-launched agents did not yet
            // promote (§1.0 rule 2 is what changed that).
            let launched_by = Some(stored.unwrap_or_else(|| {
                if mode.as_deref() == Some("promoted") || promoted_at.is_some() {
                    "user"
                } else {
                    "remuda"
                }
                .to_string()
            }));
            let role: Option<String> = row.get(18)?;
            let scope_raw: Option<String> = row.get(19)?;
            let scope: remuda_protocol::InstanceScope = scope_raw
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .unwrap_or_default();
            let project_id = scope.single_project_id().map(|id| id.as_id().to_string());
            let grants_raw: Option<String> = row.get(20)?;
            let grants: Vec<String> = grants_raw
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .unwrap_or_default();
            let task_id: Option<String> = row.get(21)?;
            // D-047: the observed route, written only from the Node create
            // echo. A malformed stored value reads as "no route" the same way
            // an absent one does; it never fails the instance load.
            let api_route: Option<remuda_protocol::ApiRoute> = row
                .get::<_, Option<String>>(22)?
                .and_then(|raw| serde_json::from_str(&raw).ok());
            // Additive context-usage rollup (context-usage-1), folded from the
            // durable usage_events table so it is never stored redundantly.
            let usage_rollup = crate::usage_store::rollup_instance(
                conn,
                id,
                &row.get::<_, String>(3)?,
                model.as_deref(),
            )?;
            Ok(InstanceRecord {
                instance_id: row.get(0)?,
                parent_instance_id: spec
                    .get("parentInstanceId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
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
                provider_source,
                provider_source_hint,
                api_route,
                model,
                permission_mode,
                tui: spec
                    .get("tui")
                    .and_then(|value| serde_json::from_value(value.clone()).ok()),
                effort_name,
                effort_ultracode,
                effort_index,
                effort_effective,
                model_effective,
                model_catalog,
                model_pin_mismatches,
                native_session_id: spec
                    .get("nativeSessionId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                native_transcript_path: spec
                    .get("nativeTranscriptPath")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                signal_tier: spec
                    .get("nativeSignalTier")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                resumed_from: spec
                    .get("resumedFrom")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                journal_id: row.get(9)?,
                durable_seq: durable.to_string(),
                configure_seq: row.get(23)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
                last_error: row.get(14)?,
                mode,
                promoted_at,
                launched_by,
                role,
                scope,
                project_id,
                grants,
                task_id,
                usage_rollup,
                lineage_id: {
                    let stamped: Option<String> = row.get(24)?;
                    stamped.unwrap_or_else(|| row.get::<_, String>(0).unwrap_or_default())
                },
                generation: row.get(25)?,
                chapter_cause: row.get(26)?,
                fenced_at: row.get(27)?,
                restart: row
                    .get::<_, Option<String>>(28)?
                    .and_then(|raw| serde_json::from_str(&raw).ok()),
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

/// Append position threaded through one (possibly batched) journal write.
///
/// Reproduces exactly how the ws layer fed the old per-event loop: the first
/// event may carry the frame's `seq` hint; every later event is handed
/// `previous_recorded_seq + 1` explicitly, whether the previous event was fresh
/// or a replay. `expected_next` is the independent durable+1 cursor the gap
/// check compares against; a replay leaves it untouched.
struct AppendCursor {
    /// Seq a gap-free fresh append must equal.
    expected_next: i64,
    /// Durable seq observed so far; advances as fresh events land.
    durable: i64,
    /// Explicit hint for the next event (None means "assign expected_next").
    next_hint: Option<i64>,
}

impl AppendCursor {
    fn new(durable: i64, first_hint: Option<i64>) -> Self {
        Self {
            expected_next: durable + 1,
            durable,
            next_hint: first_hint,
        }
    }
}

/// Apply one event to an instance the caller has loaded and host-checked.
///
/// Shared by the single [`Store::append_journal`] path and the batched
/// transaction, so the two can never drift. Works on any `&Connection` — the
/// writer's own connection or an open `Transaction`.
fn append_loaded_event(
    conn: &Connection,
    host_id: &str,
    instance_id: &str,
    cursor: &mut AppendCursor,
    mut event: Value,
) -> Result<JournalAppend, StoreError> {
    let seq = cursor.next_hint.unwrap_or(cursor.expected_next);
    if let Some(existing) = load_journal_row(conn, instance_id, seq)? {
        apply_interaction_event(conn, host_id, instance_id, &existing.event)?;
        // A replay hands the next event this existing row's seq + 1, just as
        // the old per-event loop derived its next hint from the returned
        // record. The durable cursor does not move.
        cursor.next_hint = Some(existing.seq.saturating_add(1));
        return Ok(JournalAppend {
            record: existing,
            replayed: true,
            durable_seq: cursor.durable,
            settlement: Settlement::default(),
        });
    }
    if seq != cursor.expected_next {
        return Err(StoreError::Id(format!(
            "journal gap: expected {}, got {seq}",
            cursor.expected_next
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
            .or_insert_with(|| json!(instance_id.to_string()));
    }
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO journal (instance_id, seq, event_id, payload_json, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![instance_id, seq, event_id, event.to_string(), now],
    )?;
    // Cards this event's terminal transition invalidates settle in the SAME
    // transaction; callers broadcast the returned settlement after commit
    // (c-cardsettle).
    let mut settlement = Settlement::default();
    apply_instance_projection(conn, instance_id, &event, seq, &now, &mut settlement)?;
    apply_native_session_projection(conn, instance_id, &event, &now)?;
    apply_command_projection(conn, host_id, instance_id, &event, &now)?;
    apply_interaction_event(conn, host_id, instance_id, &event)?;
    apply_instance_lifecycle(conn, instance_id, &event, &mut settlement)?;
    cursor.expected_next = seq + 1;
    cursor.durable = seq;
    cursor.next_hint = Some(seq.saturating_add(1));
    Ok(JournalAppend {
        record: JournalRecord {
            instance_id: instance_id.to_string(),
            seq,
            event_id,
            event,
            observed_at: now,
        },
        replayed: false,
        durable_seq: seq,
        settlement,
    })
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

/// D-057 §7.3: the single commit-time authority check, run INSIDE the writer
/// job that admits an Agent-initiated mutation. Passes for `None` (Human/Bot
/// callers and Hub-internal cleanup). Otherwise it requires all of:
/// - the initiator's instance exists and is not fenced;
/// - its stamped generation equals the lineage's live generation;
/// - the lineage is not paused;
/// - when a device id is given, that exact device row still exists and is
///   either bound to the initiator's instance (launch credential / MCP token)
///   or is the unbound Human device that narrowed to it
///   (`x-remuda-instance-id`). Hub-internal successor initiators pass `None`
///   and skip the device clause.
///
/// Any failure is [`StoreError::Fenced`] (409 `fenced`); the surrounding
/// writer job writes nothing because the error rolls the transaction back.
pub(crate) fn check_initiator(
    conn: &Connection,
    initiator: Option<&remuda_protocol::Initiator>,
    device_id: Option<&str>,
) -> Result<(), StoreError> {
    let Some(initiator) = initiator else {
        return Ok(());
    };
    let authority: Option<(Option<String>, i64, Option<String>)> = conn
        .query_row(
            "SELECT i.fenced_at, COALESCE(l.generation, i.generation), l.state
             FROM instances i
             LEFT JOIN lineages l ON l.lineage_id = i.lineage_id
             WHERE i.id = ?1",
            params![initiator.instance_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((fenced_at, live_generation, lineage_state)) = authority else {
        return Err(StoreError::Fenced);
    };
    if fenced_at.is_some()
        || live_generation != initiator.generation
        || lineage_state.as_deref() == Some("paused")
    {
        return Err(StoreError::Fenced);
    }
    if let Some(device_id) = device_id {
        let device: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT kind, instance_id FROM devices WHERE id = ?1",
                params![device_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let device_ok = match device {
            // The launch credential or an MCP token bound to this instance.
            Some((_, Some(bound))) => bound == initiator.instance_id,
            // An UNBOUND device that narrowed with x-remuda-instance-id:
            // caller() only allows narrowing when the presented token is not
            // itself bound, so the row is Human or Bot (the CLI's
            // REMUDA_INSTANCE_ID Bot path). Both may act for the chapter via
            // narrowing; only an unbound device can narrow at all.
            Some((_kind, None)) => true,
            None => false,
        };
        if !device_ok {
            return Err(StoreError::Fenced);
        }
    }
    Ok(())
}

/// Test-only: apply the authority effects of a fence to one instance — marks
/// the chapter fenced and bumps its lineage's live generation into `paused`.
/// The cancellations and device deletions of the real F land in ma-fence.
#[doc(hidden)]
pub(crate) fn test_apply_fence(conn: &Connection, instance_id: &str) -> Result<(), StoreError> {
    let now = now_rfc3339();
    conn.execute(
        "UPDATE instances SET fenced_at = ?1, updated_at = ?1 WHERE id = ?2",
        params![now, instance_id],
    )?;
    conn.execute(
        "UPDATE lineages
            SET generation = generation + 1, state = 'paused', paused_at = ?1, updated_at = ?1
          WHERE lineage_id = (SELECT lineage_id FROM instances WHERE id = ?2)",
        params![now, instance_id],
    )?;
    Ok(())
}

fn load_command(conn: &Connection, id: &str) -> Result<Option<CommandRecord>, StoreError> {
    conn.query_row(
        "SELECT id, instance_id, host_id, operation, state, resolution, forwarded,
                payload_json, idempotency_key, created_at, updated_at,
                settlement_outcome, settlement_reason,
                settlement_http_status, settlement_http_body,
                initiator_instance_id, initiator_lineage_id, initiator_generation,
                initiator_device_id
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
                payload_json, idempotency_key, created_at, updated_at,
                settlement_outcome, settlement_reason,
                settlement_http_status, settlement_http_body,
                initiator_instance_id, initiator_lineage_id, initiator_generation,
                initiator_device_id
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
            // c-cardsettle r5 item 8 / r6 item 4: the entity lifecycle's
            // reasonCode names WHY the interaction left pending (a transcript
            // picker demotion is agent-demoted). The real producer
            // (shell_pty promotion retire_payload) sends reasonCode WITHOUT a
            // known resolution — the Interaction entity still carries
            // resolution Unknown — so build the known resolution from
            // reasonCode when one is not already present; an existing known
            // resolution wins and is only back-filled with the reason. Carry
            // it through so reconnect/lag replay and the delete-tombstone
            // label the row correctly instead of defaulting to
            // generation-ended.
            let payload_json = match event
                .pointer("/payload/reasonCode")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
            {
                Some(reason_code) if matches!(state, "invalidated" | "expired") => {
                    let mut stamped = event.clone();
                    let already_known = stamped
                        .pointer("/payload/entity/resolution/state")
                        .and_then(Value::as_str)
                        .is_some_and(|state| state == "known");
                    if !already_known
                        && let Some(entity) = stamped
                            .pointer_mut("/payload/entity")
                            .filter(|entity| entity.is_object())
                    {
                        entity["resolution"] = json!({
                            "state": "known",
                            "value": { "reason": reason_code, "eventIds": [] }
                        });
                    } else if let Some(value) = stamped
                        .pointer_mut("/payload/entity/resolution/value")
                        .filter(|value| value.is_object())
                    {
                        value["reason"] = json!(reason_code);
                    }
                    stamped.to_string()
                }
                _ => event.to_string(),
            };
            conn.execute("UPDATE interactions SET state = ?1, blocking = 0, payload_json = ?2, updated_at = ?3 WHERE id = ?4",
                params![state, payload_json, now_rfc3339(), id])?;
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
        // c-cardsettle r2 item 6: a replayed `interaction.requested` (e.g. a
        // lost-ack replay) must not overwrite an already-terminal row. The old
        // upsert rewrote payload_json unconditionally, stripping the
        // generation-ended/invalidated resolution (and re-stamping a
        // terminal card's activity), which made the desktop mislabel it as
        // 已在其它设备处理. Keep the existing payload/state when the row is
        // already invalidated or expired; only a pending (or non-terminal) row
        // absorbs the replay.
        conn.execute(
            "INSERT INTO interactions
                (id, instance_id, host_id, kind, state, blocking, payload_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 1, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET
                payload_json = CASE
                    WHEN interactions.state IN ('invalidated', 'expired')
                        THEN interactions.payload_json
                    ELSE excluded.payload_json
                END,
                updated_at = CASE
                    WHEN interactions.state IN ('invalidated', 'expired')
                        THEN interactions.updated_at
                    ELSE excluded.updated_at
                END,
                state = CASE WHEN interactions.state = 'pending' THEN 'pending' ELSE interactions.state END",
            params![id, instance_id, host_id, ikind, event.to_string(), now],
        )?;
        // A terminal interaction never puts the instance back to blocked.
        let already_terminal: bool = conn
            .query_row(
                "SELECT state IN ('invalidated', 'expired') FROM interactions WHERE id = ?1",
                params![id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !already_terminal {
            conn.execute(
                "UPDATE instances SET activity = 'blocked', updated_at = ?1 WHERE id = ?2",
                params![now, instance_id],
            )?;
        }
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

/// Lifecycles that mean the process (or launch attempt) has ended.
fn lifecycle_is_terminal(lifecycle: &str) -> bool {
    matches!(lifecycle, "exited" | "failed" | "closed")
}

/// Lifecycles that are a real end event (ma-lineage round 2).
const ENDED_LIFECYCLES: &str = "'exited', 'failed', 'closed'";

/// Apply the `ended_at` column for an instance lifecycle write, driven by the
/// SHARED classifier's [`ProcessEnd`] evidence (ma-lineage r4 items 1+2).
///
/// * return-to-live (`ready`/`running`/`starting`) clears `ended_at` — later
///   live evidence wins over an earlier ambiguous terminal;
/// * a terminal lifecycle with classifier evidence stamps the evidence's own
///   timestamp (`end.at`, the event's observedAt) or this write clock, once
///   via COALESCE so a later row cannot rewrite the first end;
/// * a terminal lifecycle WITHOUT evidence is left untouched here. That is
///   the legacy/ambiguous case (a `failed` marked by a configure/turn error
///   while the process was alive): the row is treated as potentially live and
///   must not gain an `ended_at` that would attest a death nobody observed.
fn apply_ended_at(
    conn: &Connection,
    instance_id: &str,
    resulting_lifecycle: &str,
    end: Option<&remuda_protocol::process_end::ProcessEnd>,
    now: &str,
) -> Result<(), StoreError> {
    if matches!(resulting_lifecycle, "ready" | "running" | "starting") {
        conn.execute(
            "UPDATE instances SET ended_at = NULL WHERE id = ?1",
            params![instance_id],
        )?;
        return Ok(());
    }
    if let Some(end) = end
        && lifecycle_is_terminal(resulting_lifecycle)
    {
        let at = end
            .at
            .clone()
            .map(String::from)
            .filter(|at| !at.is_empty())
            .unwrap_or_else(|| now.to_string());
        stamp_ended_at(conn, instance_id, &at)?;
    }
    Ok(())
}

/// Stamp the immutable `ended_at` once, for a write that is process-end
/// evidence by construction (scheduler / settle / Node-rejected launch, or a
/// classified end event).
///
/// `COALESCE` keeps the first end timestamp: a close ACK landing after the
/// process-exit event (or any later journal row) must not rewrite it, and the
/// value must not track the mutable `updated_at`.
fn stamp_ended_at(conn: &Connection, instance_id: &str, at: &str) -> Result<(), StoreError> {
    conn.execute(
        &format!(
            "UPDATE instances SET ended_at = COALESCE(ended_at, ?1)
             WHERE id = ?2 AND lifecycle IN ({ENDED_LIFECYCLES})"
        ),
        params![at, instance_id],
    )?;
    Ok(())
}

/// The `last_error` marker `expire_lost_hosts` stamps when the Hub loses
/// contact with a host past grace. Host loss is CONTACT loss, never process
/// end (D-019: the process keeps running and is reconciled when the host
/// returns): such an `exited` row is treated as potentially live until it
/// carries a real `ended_at`.
/// The exact `last_error` marker the stale-create sweep stamps when a create
/// is never acknowledged by the node. Attests the launch never started.
pub(crate) const CREATE_NEVER_ACKNOWLEDGED_MARKER: &str = "create-never-acknowledged";

pub(crate) const HOST_LOST_MARKER: &str = "host-lost";

/// `last_error` markers that ATTEST a launch never started even when an older
/// row has no `ended_at` yet (ma-lineage r4 item 1).
fn is_attested_launch_failure_marker(last_error: &str) -> bool {
    // Match the Hub's OWN exact markers, not prose: the stale-create sweep
    // stamps `create-never-acknowledged` (ma-lineage r5 item 4).
    let text = last_error.to_ascii_lowercase();
    text == CREATE_NEVER_ACKNOWLEDGED_MARKER
        || text.contains("start-fail")
        || text.contains("start failed")
        || text.contains("never started")
}

/// ma-lineage r4 item 1 (OA6) pure row-level predicate for the continuation
/// gate and predecessor close: does this chapter DEFINITELY carry process-end
/// evidence?
///
/// * `exited` / `closed` — a real observed process end.
/// * `failed` — ONLY with recorded process-end evidence: `ended_at` stamped
///   from a classified end event (or a by-construction scheduler / Node
///   rejection), or an attested launch-failure marker.
///
/// An AMBIGUOUS legacy `failed` row (no ended_at, no launch-failure
/// attestation — typically marked by a configure/turn error while the process
/// was alive) is treated as POTENTIALLY LIVE: it does NOT satisfy this
/// predicate, so a sessionless continuation keeps its 409 and a live-host
/// successor still closes the predecessor.
/// Whether `record` is a LIVE current chapter whose addressed older chapter
/// should resolve as an idempotent replay (ma-lineage r6 item 4): requested/
/// starting/ready/running. A host-lost/ambiguous terminal chapter is NOT live
/// — it must be continued (and the possibly-alive predecessor closed), never
/// replayed.
pub(crate) fn current_chapter_is_live(record: &InstanceRecord) -> bool {
    matches!(
        record.lifecycle.as_str(),
        "requested" | "preparing" | "starting" | "ready" | "running"
    )
}

pub(crate) fn lifecycle_has_process_end_evidence(
    lifecycle: &str,
    last_error: Option<&str>,
    ended_at: Option<&str>,
) -> bool {
    match lifecycle {
        "closed" => true,
        "exited" => {
            // Genuine once an end time is recorded; otherwise only a
            // host-lost sweep exit (contact loss) is treated as potentially
            // live — any other/absent marker is a real exit.
            ended_at.is_some() || last_error.is_none_or(|error| error != HOST_LOST_MARKER)
        }
        "failed" => ended_at.is_some() || last_error.is_some_and(is_attested_launch_failure_marker),
        _ => false,
    }
}

/// Whether a `type=entity` lifecycle payload targets the INSTANCE entity.
///
/// An explicit `entityType` is authoritative; a bare state-only entity (the
/// driver shorthand, and several older test fixtures) with no other entity
/// key is the instance. Shared by the projection
/// (`apply_instance_projection`) and the derivation
/// (`derive_instance_state`) so both agree which entity ends the root.
fn payload_is_instance_entity(payload: &Value) -> bool {
    match payload.get("entityType").and_then(Value::as_str) {
        Some("instance") => true,
        Some(_) => false,
        None => {
            payload.get("instance").is_some()
                || !["host", "workspace", "run", "command", "interaction"]
                    .iter()
                    .any(|key| payload.get(*key).is_some())
        }
    }
}

/// c-cardsettle r3 item 8 / r4 item 3: a native lifecycle observation
/// attributed to a SUBAGENT (a non-empty `relatedIds.agentId`) belongs to
/// that subagent's row, never the main instance. `agentType` is OPTIONAL —
/// some raw producers stamp only the id — so scope is decided by agentId
/// alone. Main-session observations carry no agentId.
pub(crate) fn native_payload_is_subagent(payload: &Value) -> bool {
    let related = payload
        .get("relatedIds")
        .or_else(|| payload.get("related_ids"));
    related
        .and_then(Value::as_object)
        .and_then(|r| r.get("agentId"))
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
}

/// Terminal interaction states that no longer answer and leave the actionable
/// queue.
const TERMINAL_INSTANCE_LIFECYCLES: &[&str] = &["exited", "failed", "closed"];

/// Open a write transaction that takes the RESERVED lock immediately
/// (`BEGIN IMMEDIATE`). A read-then-write job MUST use this rather than a
/// deferred transaction: a deferred tx first acquires a SHARED lock on the
/// SELECT and then has to upgrade to EXCLUSIVE at commit, which deadlocks with
/// SQLITE_BUSY if a pooled reader still holds SHARED (busy_timeout cannot
/// resolve that upgrade). IMMEDIATE waits on the busy timeout instead.
pub(crate) fn immediate_tx(conn: &mut Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
}

/// c-cardsettle: invalidate every still-`pending` interaction owned by
/// `instance_ids` when those instances settle into a terminal lifecycle
/// (Node epoch change / restart reconcile, explicit kill/delete, exit
/// lifecycle event, host-lost sweep, failed launch).
///
/// Uses the protocol's existing terminal representation for a generation that
/// ended (docs/design/protocol.md §2.6 state machine:
/// `pending -> invalidated: 原生撤销或 generation 结束`): the durable state
/// becomes `invalidated`, `blocking` clears, and the embedded entity carries
/// `resolution.reason = generation-ended`. Runs on the CALLER's
/// connection/transaction, so the instance UPDATE and the interaction
/// settlement commit atomically — the inbox can never observe an exited
/// instance with a still-actionable card.
///
/// Returns the settled pairs. Idempotent: rows already in a non-pending state
/// (answered/resolved/expired/invalidated) are untouched, and a second
/// settlement after an instance is already terminal yields nothing.
pub(crate) fn settle_instance_interactions(
    conn: &Connection,
    instance_ids: &[String],
    now: &str,
) -> Result<Settlement, StoreError> {
    if instance_ids.is_empty() {
        return Ok(Settlement::default());
    }
    let placeholders = vec!["?"; instance_ids.len()].join(",");
    let sql = format!(
        "SELECT id, instance_id, payload_json FROM interactions
         WHERE state = 'pending' AND instance_id IN ({placeholders})"
    );
    let params: Vec<&dyn rusqlite::types::ToSql> = instance_ids
        .iter()
        .map(|id| id as &dyn rusqlite::types::ToSql)
        .collect();
    let pending: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut settlement = Settlement::default();
    for (id, owner_instance_id, payload_json) in pending {
        let mut event: Value = serde_json::from_str(&payload_json).unwrap_or_else(|_| json!({}));
        invalidate_interaction_payload(&mut event, now);
        let changed = conn.execute(
            "UPDATE interactions
                SET state = 'invalidated', blocking = 0, payload_json = ?2, updated_at = ?3
              WHERE id = ?1 AND state = 'pending'",
            params![id, event.to_string(), now],
        )?;
        if changed > 0 {
            settlement.interactions.push(SettledInteraction {
                instance_id: owner_instance_id,
                interaction_id: id,
                updated_at: now.to_owned(),
            });
        }
    }
    Ok(settlement)
}

/// Stamp an `interaction.requested` payload as a generation-ended
/// invalidation (c-cardsettle). The durable payload is the original journal
/// event; the full entity sits at /payload/entity (lifecycle shape) or
/// /payload/interaction (interaction.requested shape) — whichever exists gets
/// the same terminal markers the list projection reads. No new protocol state
/// or reason is introduced: §2.6 already defines `pending -> invalidated` for
/// a generation that ended with `resolution.reason = generation-ended`.
pub(crate) fn invalidate_interaction_payload(event: &mut Value, now: &str) {
    for pointer in ["/payload/entity", "/payload/interaction"] {
        if let Some(entity) = event.pointer_mut(pointer).and_then(Value::as_object_mut) {
            entity.insert("state".into(), json!("invalidated"));
            entity.insert("blocking".into(), json!(false));
            entity.insert("answerable".into(), json!(false));
            entity.insert(
                "resolution".into(),
                json!({
                    "state": "known",
                    "value": {
                        "reason": "generation-ended",
                        "eventIds": [],
                    },
                }),
            );
            entity.insert("updatedAt".into(), json!(now));
        }
    }
}

/// Settle pending interactions exactly once — on the transition in which the
/// EFFECTIVE stored lifecycle first becomes terminal. Both lifecycle writers
/// (the native/entity projection in `apply_instance_projection` and the
/// derived write in `apply_instance_lifecycle`) route through this, so a
/// native `exit` / severity-error that only one derivation recognises still
/// ends the generation: one terminal transition, one settle. No-op when the
/// row was already terminal (the transition's own writer did the settlement)
/// or stays non-terminal.
fn settle_on_terminal_transition(
    conn: &Connection,
    instance_id: &str,
    previous: Option<&str>,
    next: Option<&str>,
    now: &str,
    settlement: &mut Settlement,
) -> Result<(), StoreError> {
    if let Some(next) = next
        && TERMINAL_INSTANCE_LIFECYCLES.contains(&next)
        && !previous.is_some_and(|p| TERMINAL_INSTANCE_LIFECYCLES.contains(&p))
    {
        let ids = [instance_id.to_string()];
        settlement.merge(settle_instance_interactions(conn, &ids, now)?);
    }
    Ok(())
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

/// c-cardsettle r5 item 4 (OA6): whether a ROOT topic=turn native event ends
/// the TURN failed — a `result` with status/error, or a root StopFailure whose
/// `relatedIds.outcome` is "failed". Subagent scope and configure/diagnostic
/// topics are filtered by the caller before invoking this.
fn root_turn_failed(payload: &Value, native_name: &str, status: Option<&str>) -> bool {
    let outcome_failed = payload
        .pointer("/relatedIds/outcome")
        .and_then(Value::as_str)
        .is_some_and(|o| o.eq_ignore_ascii_case("failed"));
    let result_error = native_name == "result" && status == Some("error");
    let stop_failure = native_name == "stopfailure" && outcome_failed;
    result_error || stop_failure
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
/// Derive the `(lifecycle, activity)` state an event projects onto its
/// instance.
///
/// `pub(crate)` so the api-relay revocation path can recognise the same
/// terminal events (exited/failed) the projection applies, rather than
/// re-deriving the event shape in a second place.
pub(crate) fn derive_instance_state(event: &Value) -> (Option<&'static str>, Option<&'static str>) {
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .or_else(|| event.get("subtype").and_then(Value::as_str))
        .unwrap_or("");
    let payload = event.get("payload").unwrap_or(event);
    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let native_name = payload
        .get("nativeName")
        .and_then(Value::as_str)
        .unwrap_or("");
    if native_name == "SubagentStop" {
        return (None, None);
    }
    // c-cardsettle r4 item 1: a SUBAGENT-scoped event (non-empty agentId;
    // agentType is optional) belongs to that subagent's turn only. It must
    // return before BOTH the remudaActivity match and the generic status arm,
    // otherwise a subagent StopFailure carrying remudaActivity=idle would set
    // the ROOT idle mid-workflow (the owner's bug: 空闲 while the process
    // serves the workflow). Root lifecycle/activity/turn are all untouched.
    if payload_type == "native" && native_payload_is_subagent(payload) {
        return (None, None);
    }
    if payload_type == "native"
        && event.pointer("/source/driverKind").and_then(Value::as_str) == Some("shell-pty")
    {
        // Added by the Node only after current PID/session ownership was
        // verified, so activity travels on the same causal journal event.
        match payload
            .pointer("/relatedIds/remudaActivity")
            .and_then(Value::as_str)
        {
            Some("working") => return (Some("running"), Some("working")),
            Some("idle") => return (Some("running"), Some("idle")),
            _ => {}
        }
        if event.pointer("/source/channel").and_then(Value::as_str) == Some("hook") {
            return (None, None);
        }
    }
    let entity_state = payload
        .get("state")
        .and_then(Value::as_str)
        .or_else(|| event.get("state").and_then(Value::as_str));
    let status = knowledge_value(payload.get("status"))
        .or_else(|| knowledge_value(payload.pointer("/entity/activity")))
        .or_else(|| event.get("activity").and_then(Value::as_str));

    // r5 item 1: only INSTANCE-lifecycle entities fold root lifecycle;
    // workflow run/phase/member observations never fold the root (they are a
    // distinct ObservationPayload kind and never reach lifecycle derivation;
    // this gate additionally rejects a command/interaction entity). An
    // INSTANCE entity lifecycle serialises with a flattened top-level
    // "instance" key (LifecycleEntity::Instance, serde tag="instance"). Bare
    // entity events with only a state (driver shorthand / tests) and no other
    // entity key are treated as the instance.
    let is_instance_entity = payload_type == "entity" && payload_is_instance_entity(payload);
    let subagent_scoped = payload_type == "native" && native_payload_is_subagent(payload);
    let topic = payload.get("topic").and_then(Value::as_str).unwrap_or("");

    // ma-lineage r4: process-end via the SHARED classifier. Native event →
    // process_end_value(); instance entity → entity_process_end(). Anything
    // else (turn result/StopFailure, configure/diagnostic, subagent, workflow
    // member) is NOT a process end.
    let native_end: Option<remuda_protocol::process_end::ProcessEnd> =
        if payload_type == "native" && !subagent_scoped {
            remuda_protocol::process_end::process_end_value(payload)
        } else {
            None
        };
    if let Some(end) = native_end {
        return (Some(end.lifecycle()), None);
    }
    let entity_end = if is_instance_entity {
        // The gate above already decided this is the INSTANCE entity (explicit
        // entityType or a bare state-only driver shorthand); classify it as
        // such regardless of whether entityType was carried.
        remuda_protocol::process_end::entity_process_end(Some("instance"), entity_state)
    } else {
        None
    };
    if let Some(end) = entity_end {
        return (Some(end.lifecycle()), None);
    }

    // c-cardsettle r5 item 4 (OA6): a ROOT (non-subagent) `topic=turn`
    // result/status error ENDS THE TURN with outcome failed: activity becomes
    // idle (composer retryable), lifecycle stays running. A subagent,
    // configure or diagnostic error is excluded (own scope).
    if payload_type == "native"
        && topic == "turn"
        && !subagent_scoped
        && root_turn_failed(payload, native_name, status)
    {
        return (Some("running"), Some("idle"));
    }

    if kind == "interaction.requested" || kind == "interactionRequested" {
        return (Some("running"), Some("blocked"));
    }

    let mut lifecycle = None;
    let mut activity = None;
    // c-cardsettle r5 item 1 (OA6): root lifecycle/activity derive ONLY from
    // INSTANCE-lifecycle entities. A workflow run/phase/member entity — e.g. a
    // workflow.member state=failed — belongs to that member's row, never the
    // root. Non-terminal instance entity states (ready/running/idle/…) still
    // project activity; terminal ones returned above via the classifier.
    if is_instance_entity && let Some(state) = entity_state {
        lifecycle = normalize_lifecycle(state);
    }
    let herdr_idle_proof = is_instance_entity
        || native_name == "agent_status"
        || native_name == "session"
        || payload_type == "native";
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
            // ma-lineage r4: exited/failed STATUS is process-end and is
            // returned above through the shared classifier; never infer a
            // terminal lifecycle from a bare status here (a turn/configure
            // error reaches this arm while the process is alive).
            _ => {}
        }
    }
    (lifecycle, activity)
}

fn apply_instance_lifecycle(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
    settlement: &mut Settlement,
) -> Result<(), StoreError> {
    let (next_life, mut next_act) = derive_instance_state(event);
    if next_life.is_none() && next_act.is_none() {
        return Ok(());
    }
    let Some(current) = load_instance(conn, instance_id)? else {
        return Ok(());
    };
    if matches!(event.pointer("/payload/nativeName").and_then(Value::as_str), Some("agent_status" | "session"))
        && conn.query_row(
            "SELECT json_extract(spec_json, '$.nativeSignalTier') = 'hook' FROM instances WHERE id = ?1",
            params![instance_id], |row| row.get::<_, Option<bool>>(0),
        )?.unwrap_or(false)
    {
        next_act = None;
    }
    let now = now_rfc3339();
    let lifecycle = match next_life {
        Some(next) if lifecycle_rank(next) >= lifecycle_rank(&current.lifecycle) => next,
        _ => current.lifecycle.as_str(),
    };
    // c-cardsettle r2 item 6: a terminal instance never has its activity
    // revived to blocked by a late/replayed interaction.requested.
    let is_terminal = matches!(lifecycle, "exited" | "failed" | "closed");
    let activity = if is_terminal && next_act == Some("blocked") {
        current.activity.as_str()
    } else {
        next_act.unwrap_or(current.activity.as_str())
    };
    conn.execute(
        "UPDATE instances SET lifecycle = ?1, activity = ?2, updated_at = ?3 WHERE id = ?4",
        params![lifecycle, activity, now, instance_id],
    )?;
    // ma-lineage r4: the derived write settles pending cards through the SAME
    // transition guard the projection write uses. ended_at is stamped by the
    // projection (which runs earlier in this append and carries the shared
    // classifier's ProcessEnd evidence); `current` is read after it, so a
    // terminal the projection already wrote/settled is a no-op, while a
    // terminal only THIS derivation recognises still settles its generation.
    settle_on_terminal_transition(
        conn,
        instance_id,
        Some(current.lifecycle.as_str()),
        Some(lifecycle),
        &now,
        settlement,
    )?;
    Ok(())
}

fn command_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRecord> {
    let payload: String = row.get(7)?;
    let forwarded: i64 = row.get(6)?;
    let settlement_outcome: Option<String> = row.get(11)?;
    let settlement_reason: Option<String> = row.get(12)?;
    let settlement_http_status: Option<i64> = row.get(13)?;
    let settlement_http_body: Option<String> = row.get(14)?;
    // D-057 §7.1: the three initiator columns are all NULL (legacy/Human/Bot
    // rows) or all set together.
    let initiator_instance_id: Option<String> = row.get(15)?;
    let initiator_lineage_id: Option<String> = row.get(16)?;
    let initiator_generation: Option<i64> = row.get(17)?;
    let initiator = match (
        initiator_instance_id,
        initiator_lineage_id,
        initiator_generation,
    ) {
        (Some(instance_id), Some(lineage_id), Some(generation)) => {
            Some(remuda_protocol::Initiator {
                instance_id,
                lineage_id,
                generation,
            })
        }
        _ => None,
    };
    Ok(CommandRecord {
        command_id: row.get(0)?,
        instance_id: row.get(1)?,
        host_id: row.get(2)?,
        operation: row.get(3)?,
        state: row.get(4)?,
        resolution: row.get(5)?,
        forwarded: forwarded != 0,
        settlement: CommandRecord::settlement_projection(
            settlement_outcome.as_deref(),
            settlement_reason.as_deref(),
        ),
        settlement_outcome,
        settlement_reason,
        settlement_http_status,
        settlement_http_body,
        initiator,
        initiator_device_id: row.get(18)?,
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
        // r5 item 1: only entityType=instance entities fold root lifecycle.
        let (life, act) = derive_instance_state(&json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity", "entityType": "instance",
                "state": "ready", "reasonCode": "driver-started"
            }
        }));
        assert_eq!(life, Some("running"));
        assert_eq!(act, None);
        // A non-instance entity (workflow member) does NOT fold.
        let (life2, _) = derive_instance_state(&json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity", "entityType": "workflow.member",
                "state": "ready"
            }
        }));
        assert_eq!(life2, None, "a workflow.member entity never folds the root");
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
        // r5: an INSTANCE entity failed is process end; a workflow.member
        // failed is not.
        let (life, _) = derive_instance_state(&json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity", "entityType": "instance",
                "state": "failed",
                "reasonCode": "native-driver-start-failed"
            }
        }));
        assert_eq!(life, Some("failed"));
        let (life2, _) = derive_instance_state(&json!({
            "kind": "lifecycle",
            "payload": {
                "type": "entity", "entityType": "workflow.member",
                "state": "failed"
            }
        }));
        assert_eq!(life2, None, "a workflow.member failed never fails the root");
    }

    #[test]
    fn append_chunks_bound_count_and_bytes_and_cover_everything() {
        use super::{APPEND_CHUNK_MAX, APPEND_CHUNK_MAX_BYTES, journal_append_chunks};
        use serde_json::Value;
        // 256 small events split into 4 count-bounded chunks, contiguously.
        let small: Vec<Value> = (0..256).map(|n| json!({ "n": n })).collect();
        let ranges = journal_append_chunks(&small);
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges[0], 0..64);
        assert_eq!(ranges[3], 192..256);
        assert_eq!(ranges.last().unwrap().end, small.len());
        for range in &ranges {
            assert!(range.len() <= APPEND_CHUNK_MAX);
        }

        // Five huge events (each ~400 KB > no, under 1 MiB each): two fit under
        // the byte budget, so 64-large-event batches cannot be one job.
        let huge: Vec<Value> = (0..5)
            .map(|n| json!({ "blob": "q".repeat(400_000), "n": n }))
            .collect();
        let ranges = journal_append_chunks(&huge);
        for range in &ranges {
            let bytes: usize = huge[range.clone()]
                .iter()
                .map(|event| event.to_string().len())
                .sum();
            // A chunk of >1 carries at most the byte budget; a single oversized
            // event is allowed through on its own.
            if range.len() > 1 {
                assert!(bytes <= APPEND_CHUNK_MAX_BYTES, "chunk bytes {bytes}");
            }
        }
        assert_eq!(
            ranges.iter().map(std::ops::Range::len).sum::<usize>(),
            huge.len()
        );

        // One event larger than the whole budget still forms a chunk of one.
        let giant = vec![json!({ "blob": "z".repeat(2_000_000) })];
        let ranges = journal_append_chunks(&giant);
        assert_eq!(ranges, vec![0..1]);
    }
}
